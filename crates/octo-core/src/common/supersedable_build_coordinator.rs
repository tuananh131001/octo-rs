//! Port of `Services/Common/SupersedableBuildCoordinator.cs`.
//!
//! Wraps a [`SingleFlight`] keyed build with prefix-based supersession for interactive
//! type-ahead search: a shorter, still-running query is cancelled the moment a longer query
//! that extends it (or vice versa) arrives, since the shorter one's result is about to be
//! replaced in the client's own UI anyway.
//!
//! This exists because Amperfy (and most Subsonic clients' type-ahead) fires one search3
//! call per keystroke, uncancelled. Each partial-word build competed for the same
//! rate-limited external lane as the query the user actually meant, and the real query
//! could blow its own timeout waiting behind three stale ones. Cancelling a superseded
//! build frees that lane immediately instead of waiting out its own timeout.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::dotnet::to_lower_invariant;
use super::single_flight::SingleFlight;

pub struct SupersedableBuildCoordinator<V> {
    active_builds: Arc<Mutex<HashMap<String, CancellationToken>>>,
    flight: SingleFlight<String, V>,
}

impl<V> Default for SupersedableBuildCoordinator<V> {
    fn default() -> Self {
        Self {
            active_builds: Arc::new(Mutex::new(HashMap::new())),
            flight: SingleFlight::default(),
        }
    }
}

/// Takes a build's token out of the active set when the build ends, however it ends.
struct ActiveBuild {
    active_builds: Arc<Mutex<HashMap<String, CancellationToken>>>,
    key: String,
}

impl Drop for ActiveBuild {
    fn drop(&mut self) {
        self.active_builds.lock().remove(&self.key);
    }
}

impl<V> SupersedableBuildCoordinator<V>
where
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `factory` for `query`, cancelling any still-running build for a shorter or longer
    /// query that shares a prefix with this one, then join or start this query's own build.
    ///
    /// `fallback` is returned if the build fails or times out, instead of throwing.
    /// `on_failure` is invoked with the query and error on failure, for logging.
    ///
    /// The supersession and the join happen before this returns, as they did synchronously
    /// inside the C# `RunAsync` before its first await.
    pub fn run<F, Fut, OnFailure>(
        &self,
        query: &str,
        factory: F,
        timeout: Duration,
        fallback: V,
        on_failure: OnFailure,
    ) -> impl Future<Output = V> + Send + 'static
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = anyhow::Result<V>> + Send + 'static,
        OnFailure: FnOnce(&str, &anyhow::Error) + Send + 'static,
    {
        let key = to_lower_invariant(query);

        // Cancel any in-flight build whose key is a prefix of this one or vice versa.
        // Never cancel a build another caller has already joined: that caller's request
        // is not superseded just because this one also started, and killing it would fail
        // a search that has nothing to do with type-ahead churn.
        let snapshot: Vec<(String, CancellationToken)> = self
            .active_builds
            .lock()
            .iter()
            .map(|(k, token)| (k.clone(), token.clone()))
            .collect();
        for (active_key, token) in snapshot {
            if active_key == key {
                continue;
            }

            let is_prefix_of_this = active_key.len() < key.len() && key.starts_with(&active_key);
            let this_is_prefix_of_it = key.len() < active_key.len() && active_key.starts_with(&key);

            if (is_prefix_of_this || this_is_prefix_of_it) && self.flight.join_count(&active_key) <= 1 {
                // Cancelling a token whose build already finished is harmless, as the C#
                // caught ObjectDisposedException for.
                token.cancel();
            }
        }

        let active_builds = Arc::clone(&self.active_builds);
        let build_key = key.clone();
        let build = self.flight.run(
            key,
            move |token: CancellationToken| {
                // Only the caller that actually starts the build reaches here and registers a
                // token to cancel: a joiner returns from SingleFlight::run's early-join path
                // and never runs this factory at all.
                let linked = token.child_token();
                active_builds.lock().insert(build_key.clone(), linked.clone());
                let registration = ActiveBuild {
                    active_builds,
                    key: build_key,
                };
                let work = factory(linked);
                async move {
                    let _registration = registration;
                    work.await
                }
            },
            timeout,
        );

        let query = query.to_string();
        async move {
            match build.await {
                Ok(value) => value,
                Err(error) => {
                    on_failure(&query, &error);
                    fallback
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    // The coordinator's join guard is what stops "cage", "cage t" from cancelling a build a
    // second caller already joined. Get that guard wrong and a client sharing a build with
    // someone else's type-ahead sees its own search cancelled for a query it never typed.

    const GENEROUS: Duration = Duration::from_secs(30);

    #[derive(Clone)]
    struct Gate(watch::Receiver<bool>, Arc<watch::Sender<bool>>);

    impl Gate {
        fn new() -> Self {
            let (sender, receiver) = watch::channel(false);
            Self(receiver, Arc::new(sender))
        }
        fn open(&self) {
            self.1.send_replace(true);
        }
        async fn wait(mut self) {
            let _ = self.0.wait_for(|open| *open).await;
        }
        /// `release.Task.WaitAsync(ct)`: the gate, or a cancellation error.
        async fn wait_or_cancelled(self, token: CancellationToken) -> anyhow::Result<()> {
            tokio::select! {
                () = self.wait() => Ok(()),
                () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
            }
        }
    }

    fn ignore(_: &str, _: &anyhow::Error) {}

    #[tokio::test]
    async fn a_joined_build_survives_supersession() {
        let coordinator = SupersedableBuildCoordinator::<i32>::new();
        let started = Gate::new();
        let release = Gate::new();

        let cage_factory = || {
            let (started, release) = (started.clone(), release.clone());
            move |token: CancellationToken| async move {
                started.open();
                release.wait_or_cancelled(token).await?;
                Ok(100)
            }
        };

        let owner = coordinator.run("cage", cage_factory(), GENEROUS, -1, ignore);
        started.clone().wait().await;

        // Joins the same build. SingleFlight increments its join counter before this call
        // returns, so the join guard sees it below.
        let joiner = coordinator.run("cage", cage_factory(), GENEROUS, -1, ignore);

        let extend = coordinator.run("cage t", |_| async { Ok(200) }, GENEROUS, -2, ignore);

        release.open();
        let results = tokio::join!(owner, joiner, extend);

        assert_eq!(results, (100, 100, 200));
    }

    #[tokio::test]
    async fn a_lone_build_is_superseded_and_falls_back_without_throwing() {
        let coordinator = SupersedableBuildCoordinator::<i32>::new();
        let started = Gate::new();

        let cage_started = started.clone();
        let cage = coordinator.run(
            "cage",
            move |token: CancellationToken| async move {
                cage_started.open();
                tokio::select! {
                    () = tokio::time::sleep(GENEROUS) => Ok(1),
                    () = token.cancelled() => Err(anyhow::anyhow!("A task was canceled.")),
                }
            },
            GENEROUS,
            -1,
            ignore,
        );
        started.clone().wait().await;

        let cage_t = coordinator.run("cage t", |_| async { Ok(2) }, GENEROUS, -2, ignore);

        assert_eq!(cage.await, -1);
        assert_eq!(cage_t.await, 2);
    }

    #[tokio::test]
    async fn an_unrelated_query_does_not_cancel_a_non_prefix_build() {
        let coordinator = SupersedableBuildCoordinator::<i32>::new();
        let started = Gate::new();
        let release = Gate::new();

        let (cage_started, cage_release) = (started.clone(), release.clone());
        let cage = coordinator.run(
            "cage",
            move |token: CancellationToken| async move {
                cage_started.open();
                cage_release.wait_or_cancelled(token).await?;
                Ok(1)
            },
            GENEROUS,
            -1,
            ignore,
        );
        started.clone().wait().await;

        let beths = coordinator
            .run("beths", |_| async { Ok(2) }, GENEROUS, -2, ignore)
            .await;
        assert_eq!(beths, 2);

        release.open();
        assert_eq!(cage.await, 1);
    }

    #[tokio::test]
    async fn failures_are_reported_with_the_query_as_typed() {
        // Rust-only: on_failure gets the caller's query, not the lowercased key.
        let coordinator = SupersedableBuildCoordinator::<i32>::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        let value = coordinator
            .run(
                "Cage",
                |_| async { Err(anyhow::anyhow!("down")) },
                GENEROUS,
                -1,
                move |query, error| sender.send(format!("{query}: {error}")).expect("receiver alive"),
            )
            .await;
        assert_eq!(value, -1);
        assert_eq!(receiver.recv().expect("reported"), "Cage: down");
    }
}
