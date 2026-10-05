//! Port of `Services/Common/SingleFlight.cs`.
//!
//! Collapses concurrent identical work onto one execution. Callers arriving while a key
//! is already running join the running task instead of starting their own.
//!
//! This holds no results. Once the work finishes the entry is gone, so there is no
//! staleness policy, no negative-caching rule, and nothing for a poisoned entry to
//! survive in. That is deliberate: the failure mode we are avoiding is a keyed dictionary
//! of tasks that outlives its usefulness, which on arbitrary user input becomes a
//! user-triggerable leak.
//!
//! Like a C# `Task`, the work is hot: [`SingleFlight::run`] registers the caller (or starts
//! the work on a tokio task) before it returns, and the work runs to the end even if the
//! caller drops the returned future. Only awaiting the answer is lazy.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::FutureExt;
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A failure every joined caller sees, as the C# rethrew the one exception to each awaiter.
pub type SharedError = Arc<anyhow::Error>;

type Outcome<V> = Option<Result<V, SharedError>>;

struct Entry<V> {
    outcome: watch::Receiver<Outcome<V>>,

    /// Callers currently sharing this execution, including the owner. A supersession
    /// policy (see [`SupersedableBuildCoordinator`](super::supersedable_build_coordinator::SupersedableBuildCoordinator))
    /// reads this to decide whether cancelling this key would only hurt its own caller or
    /// would also take a joined caller down with it.
    joins: AtomicUsize,
}

pub struct SingleFlight<K, V> {
    in_flight: Arc<Mutex<HashMap<K, Arc<Entry<V>>>>>,
}

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of executions currently running. Exposed for tests and logging.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.lock().len()
    }

    /// How many callers are currently sharing the execution for `key`, or 0 if none is running.
    pub fn join_count(&self, key: &K) -> usize {
        self.in_flight
            .lock()
            .get(key)
            .map_or(0, |entry| entry.joins.load(Ordering::SeqCst))
    }

    /// Run `factory` for `key`, or join the execution already running for it.
    ///
    /// `timeout` is the deadline for the work itself. The factory never sees a caller's
    /// cancellation token: joined callers share one execution, so letting the first one to
    /// disconnect cancel it would take everyone else down with it. A deadline is what stops a
    /// hung dependency pinning the key instead. (As in C#, the deadline only cancels the token
    /// the factory is given; the factory decides how to stop.)
    ///
    /// The owner's factory is called before this returns, so it can register itself
    /// synchronously, as the C# factory ran up to its first await inside `RunAsync`. A factory
    /// that panics, while being called or while running, fails every caller like a thrown
    /// exception and is not retained.
    pub fn run<F, Fut>(
        &self,
        key: K,
        factory: F,
        timeout: Duration,
    ) -> impl Future<Output = Result<V, SharedError>> + Send + 'static
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
    {
        let (sender, receiver) = watch::channel::<Outcome<V>>(None);
        {
            let mut in_flight = self.in_flight.lock();
            if let Some(existing) = in_flight.get(&key) {
                // Someone else owns this key. Join them rather than doing the work twice.
                existing.joins.fetch_add(1, Ordering::SeqCst);
                return Self::await_outcome(existing.outcome.clone()).left_future();
            }
            in_flight.insert(
                key.clone(),
                Arc::new(Entry {
                    outcome: receiver.clone(),
                    joins: AtomicUsize::new(1),
                }),
            );
        }

        let token = CancellationToken::new();
        let in_flight = Arc::clone(&self.in_flight);
        let finish = move |outcome: Result<V, SharedError>| {
            // Every joiner sees the outcome, and the entry is removed after, so the next
            // caller retries rather than inheriting a permanently faulted task.
            sender.send_replace(Some(outcome));
            // After the result is set, not before: a caller arriving in between joins a
            // completed task and gets the answer, where the reverse order would have it
            // start a redundant execution.
            in_flight.lock().remove(&key);
        };

        match std::panic::catch_unwind(AssertUnwindSafe(|| factory(token.clone()))) {
            Err(panic) => finish(Err(Arc::new(panic_error(panic)))),
            Ok(work) => {
                tokio::spawn(async move {
                    let deadline = {
                        let token = token.clone();
                        async move {
                            tokio::time::sleep(timeout).await;
                            token.cancel();
                        }
                    };
                    let work = AssertUnwindSafe(work).catch_unwind();
                    tokio::pin!(work);
                    tokio::pin!(deadline);
                    let result = tokio::select! {
                        result = &mut work => result,
                        () = &mut deadline => work.await,
                    };
                    finish(match result {
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(error)) => Err(Arc::new(error)),
                        Err(panic) => Err(Arc::new(panic_error(panic))),
                    });
                });
            }
        }
        Self::await_outcome(receiver).right_future()
    }

    async fn await_outcome(mut receiver: watch::Receiver<Outcome<V>>) -> Result<V, SharedError> {
        match receiver.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().expect("waited for a value"),
            // Only when the runtime shut down under the work.
            Err(_) => Err(Arc::new(anyhow::anyhow!(
                "the shared execution was abandoned before it finished"
            ))),
        }
    }
}

fn panic_error(panic: Box<dyn std::any::Any + Send>) -> anyhow::Error {
    let message = panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "the work panicked".to_string());
    anyhow::anyhow!(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::oneshot;

    // Single-flight is what stops one typed query from running the discovery pipeline several
    // times over. The dangerous part is not the happy path but what a failure leaves behind:
    // a keyed dictionary of tasks that outlives its usefulness becomes a user-triggerable
    // leak, and a retained faulted task turns one bad moment into a permanently broken query.

    const GENEROUS: Duration = Duration::from_secs(30);

    /// A one-shot gate the test opens, cloneable into every factory (TaskCompletionSource).
    #[derive(Clone)]
    struct Gate(
        tokio::sync::watch::Receiver<bool>,
        Arc<tokio::sync::watch::Sender<bool>>,
    );

    impl Gate {
        fn new() -> Self {
            let (sender, receiver) = tokio::sync::watch::channel(false);
            Self(receiver, Arc::new(sender))
        }
        fn open(&self) {
            self.1.send_replace(true);
        }
        async fn wait(mut self) {
            let _ = self.0.wait_for(|open| *open).await;
        }
    }

    #[tokio::test]
    async fn concurrent_callers_for_one_key_share_a_single_execution() {
        let flight = SingleFlight::<String, i32>::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Gate::new();
        let started = Gate::new();

        let factory = || {
            let (runs, release, started) = (runs.clone(), release.clone(), started.clone());
            move |_token: CancellationToken| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                started.open();
                release.wait().await;
                Ok(42)
            }
        };

        // Hold the first call open so the others cannot miss it by finishing too early.
        let first = flight.run("q".to_string(), factory(), GENEROUS);
        started.clone().wait().await;

        let joiners: Vec<_> = (0..8)
            .map(|_| flight.run("q".to_string(), factory(), GENEROUS))
            .collect();

        release.open();
        let mut results = vec![first.await.expect("ok")];
        for joiner in joiners {
            results.push(joiner.await.expect("ok"));
        }

        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|&r| r == 42));
    }

    #[tokio::test]
    async fn different_keys_do_not_share_an_execution() {
        let flight = SingleFlight::<String, String>::new();

        let a = flight.run("a".to_string(), |_| async { Ok("a".to_string()) }, GENEROUS);
        let b = flight.run("b".to_string(), |_| async { Ok("b".to_string()) }, GENEROUS);
        let (a, b) = tokio::join!(a, b);

        assert_eq!([a.expect("ok"), b.expect("ok")], ["a", "b"]);
    }

    #[tokio::test]
    async fn a_failed_execution_reaches_every_joiner_and_is_not_retained() {
        let flight = SingleFlight::<String, i32>::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Gate::new();
        let started = Gate::new();

        let failing = || {
            let (runs, release, started) = (runs.clone(), release.clone(), started.clone());
            move |_token: CancellationToken| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                started.open();
                release.wait().await;
                Err::<i32, _>(anyhow::anyhow!("upstream is down"))
            }
        };

        let first = flight.run("q".to_string(), failing(), GENEROUS);
        started.clone().wait().await;
        let joiner = flight.run("q".to_string(), failing(), GENEROUS);

        release.open();

        assert_eq!(first.await.expect_err("fails").to_string(), "upstream is down");
        assert_eq!(joiner.await.expect_err("fails").to_string(), "upstream is down");
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // The entry must be gone, or one transient outage would poison this key for the
        // life of the process.
        assert_eq!(flight.in_flight_count(), 0);
        assert_eq!(
            flight
                .run("q".to_string(), |_| async { Ok(7) }, GENEROUS)
                .await
                .expect("ok"),
            7
        );
    }

    #[tokio::test]
    async fn a_synchronously_throwing_factory_is_also_not_retained() {
        let flight = SingleFlight::<String, i32>::new();

        let thrown = flight
            .run(
                "q".to_string(),
                |_| -> std::future::Ready<anyhow::Result<i32>> { panic!("boom") },
                GENEROUS,
            )
            .await;
        assert_eq!(thrown.expect_err("fails").to_string(), "boom");

        assert_eq!(flight.in_flight_count(), 0);
        assert_eq!(
            flight
                .run("q".to_string(), |_| async { Ok(1) }, GENEROUS)
                .await
                .expect("ok"),
            1
        );
    }

    #[tokio::test]
    async fn a_completed_key_runs_again_on_the_next_call() {
        let flight = SingleFlight::<String, usize>::new();
        let runs = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let runs = runs.clone();
            flight
                .run(
                    "q".to_string(),
                    move |_| async move { Ok(runs.fetch_add(1, Ordering::SeqCst) + 1) },
                    GENEROUS,
                )
                .await
                .expect("ok");
        }

        // Nothing is cached, so results stay fresh and no eviction policy is needed.
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(flight.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn the_factory_token_is_cancelled_by_the_deadline() {
        let flight = SingleFlight::<String, bool>::new();

        // A hung dependency must not pin the key. The token the factory receives belongs to
        // the execution, never to a caller: joined callers share it, so one client
        // disconnecting cannot take the others down with it.
        let cancelled = flight
            .run(
                "q".to_string(),
                |token: CancellationToken| async move {
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_secs(30)) => Ok(false),
                        () = token.cancelled() => Ok(true),
                    }
                },
                Duration::from_millis(150),
            )
            .await
            .expect("ok");

        assert!(cancelled);
        assert_eq!(flight.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn a_dropped_caller_does_not_abandon_the_work_or_its_joiners() {
        // Rust-only: a C# Task runs whether or not anyone awaits it. Dropping the owner's
        // future must leave the work running for the joiner and clean up after.
        let flight = SingleFlight::<String, i32>::new();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let owner = flight.run(
            "q".to_string(),
            |_| async move {
                let _ = release_rx.await;
                Ok(5)
            },
            GENEROUS,
        );
        let joiner = flight.run("q".to_string(), |_| async { Ok(0) }, GENEROUS);
        assert_eq!(flight.join_count(&"q".to_string()), 2);
        drop(owner);
        let _ = release_tx.send(());
        assert_eq!(joiner.await.expect("ok"), 5);
        assert_eq!(flight.in_flight_count(), 0);
    }
}
