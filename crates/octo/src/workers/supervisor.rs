//! The background workers' host: what ASP.NET's `IHostedService` / `BackgroundService` plumbing
//! did for the C# workers registered with `AddHostedService` in `Program.cs`.
//!
//! A worker is a named async function taking a [`CancellationToken`] (the `stoppingToken` of
//! `ExecuteAsync`). The supervisor starts every worker when the app starts, cancels them all on
//! shutdown and waits for them up to the shutdown budget (`HostOptions.ShutdownTimeout`, 10 s in
//! `Program.cs`), then abandons the stragglers.
//!
//! **Failure policy.** In .NET 8+, an exception escaping `ExecuteAsync` stops the whole host
//! (`BackgroundServiceExceptionBehavior.StopHost`), and Docker's restart policy then restarts
//! the container. Octo never relied on that: every C# worker wraps each item or tick in a catch
//! ("Per-item catch is mandatory: BackgroundServiceExceptionBehavior defaults to StopHost, so a
//! single unhandled exception here would take Octo down"). Here a worker that panics or returns
//! an error is logged and restarted on its own, after a backoff, and the server keeps running;
//! see known-diffs.md. A worker that returns `Ok(())` has finished, as an `ExecuteAsync` that
//! completes does, and is not restarted.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// `HostOptions.ShutdownTimeout`: how long shutdown waits for the workers (and the server's
/// in-flight requests) before giving up on them.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The body of a worker: called once per start, with a token that is cancelled on shutdown.
pub type WorkerFn = Arc<dyn Fn(CancellationToken) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// How long to wait before restarting a failed worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failure.
    pub initial: Duration,
    /// The longest wait; each further consecutive failure doubles the previous one up to this.
    pub max: Duration,
    /// A run that lasted at least this long before failing counts as healthy, so the next
    /// failure starts again from `initial`.
    pub reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            reset_after: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The wait before restart number `consecutive_failures` (1-based).
    pub fn delay(&self, consecutive_failures: u32) -> Duration {
        let factor = 2u32.saturating_pow(consecutive_failures.saturating_sub(1));
        self.initial.saturating_mul(factor).min(self.max)
    }
}

/// What a worker is doing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Registered; the supervisor has not been started.
    Registered,
    Running,
    /// Failed and waiting to be restarted.
    BackingOff,
    /// Returned `Ok(())`, or was stopped by shutdown.
    Stopped,
}

/// A snapshot of one worker, for logs, tests and a future status endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStatus {
    pub name: String,
    pub state: WorkerState,
    /// How many times it has failed (panicked or returned an error).
    pub failures: u32,
}

/// What shutdown managed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Workers that stopped within the budget.
    pub stopped: Vec<String>,
    /// Workers still running at the deadline, which were aborted.
    pub abandoned: Vec<String>,
}

struct Worker {
    name: String,
    run: WorkerFn,
    shared: Arc<Mutex<Shared>>,
    /// The supervision loop, once started.
    handle: Option<JoinHandle<()>>,
}

struct Shared {
    state: WorkerState,
    failures: u32,
    /// The current run's task, so shutdown can abort it at the deadline.
    current: Option<AbortHandle>,
}

/// Starts, restarts and stops the background workers.
pub struct WorkerSupervisor {
    backoff: Backoff,
    root: CancellationToken,
    inner: Mutex<Inner>,
}

struct Inner {
    started: bool,
    workers: Vec<Worker>,
}

impl Default for WorkerSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WorkerSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerSupervisor")
            .field("workers", &self.status())
            .finish()
    }
}

impl WorkerSupervisor {
    pub fn new() -> Self {
        Self::with_backoff(Backoff::default())
    }

    pub fn with_backoff(backoff: Backoff) -> Self {
        WorkerSupervisor {
            backoff,
            root: CancellationToken::new(),
            inner: Mutex::new(Inner {
                started: false,
                workers: Vec::new(),
            }),
        }
    }

    /// Registers a worker (`AddHostedService`). Registering after [`WorkerSupervisor::start`]
    /// starts it at once. Must be called from within a tokio runtime once started.
    pub fn register<F, Fut>(&self, name: impl Into<String>, run: F)
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let run: WorkerFn = Arc::new(move |token| Box::pin(run(token)));
        let mut worker = Worker {
            name: name.into(),
            run,
            shared: Arc::new(Mutex::new(Shared {
                state: WorkerState::Registered,
                failures: 0,
                current: None,
            })),
            handle: None,
        };
        let mut inner = self.inner.lock();
        if inner.started {
            worker.handle = Some(self.spawn(&worker));
        }
        inner.workers.push(worker);
    }

    /// Starts every registered worker. Starting twice does nothing more.
    pub fn start(&self) {
        let mut inner = self.inner.lock();
        if inner.started {
            return;
        }
        inner.started = true;
        for i in 0..inner.workers.len() {
            let handle = self.spawn(&inner.workers[i]);
            inner.workers[i].handle = Some(handle);
        }
    }

    /// A token cancelled when shutdown begins, for work outside the registered workers.
    pub fn stopping_token(&self) -> CancellationToken {
        self.root.child_token()
    }

    pub fn status(&self) -> Vec<WorkerStatus> {
        self.inner
            .lock()
            .workers
            .iter()
            .map(|w| {
                let s = w.shared.lock();
                WorkerStatus {
                    name: w.name.clone(),
                    state: s.state,
                    failures: s.failures,
                }
            })
            .collect()
    }

    /// Cancels every worker and waits for them until `budget` runs out; whatever is still
    /// running then is aborted and named in the report.
    pub async fn shutdown(&self, budget: Duration) -> ShutdownReport {
        self.shutdown_until(Instant::now() + budget).await
    }

    /// [`WorkerSupervisor::shutdown`] against a deadline shared with other shutdown work.
    pub async fn shutdown_until(&self, deadline: Instant) -> ShutdownReport {
        self.root.cancel();
        let handles: Vec<(String, Arc<Mutex<Shared>>, Option<JoinHandle<()>>)> = {
            let mut inner = self.inner.lock();
            inner
                .workers
                .iter_mut()
                .map(|w| (w.name.clone(), w.shared.clone(), w.handle.take()))
                .collect()
        };
        let mut report = ShutdownReport::default();
        for (name, shared, handle) in handles {
            let Some(mut handle) = handle else {
                report.stopped.push(name);
                continue;
            };
            match tokio::time::timeout_at(deadline, &mut handle).await {
                Ok(_) => report.stopped.push(name),
                Err(_) => {
                    warn!("Background worker {name} did not stop within the shutdown timeout; abandoning it");
                    if let Some(current) = shared.lock().current.take() {
                        current.abort();
                    }
                    handle.abort();
                    shared.lock().state = WorkerState::Stopped;
                    report.abandoned.push(name);
                }
            }
        }
        report
    }

    fn spawn(&self, worker: &Worker) -> JoinHandle<()> {
        let name = worker.name.clone();
        let run = worker.run.clone();
        let shared = worker.shared.clone();
        let root = self.root.clone();
        let backoff = self.backoff;
        tokio::spawn(supervise(name, run, shared, root, backoff))
    }
}

/// One worker's life: run it, and on a failure wait and run it again, until it finishes or
/// shutdown begins.
async fn supervise(
    name: String,
    run: WorkerFn,
    shared: Arc<Mutex<Shared>>,
    root: CancellationToken,
    backoff: Backoff,
) {
    let mut consecutive = 0u32;
    loop {
        if root.is_cancelled() {
            break;
        }
        let started = Instant::now();
        let task = tokio::spawn(run(root.child_token()));
        {
            let mut s = shared.lock();
            s.state = WorkerState::Running;
            s.current = Some(task.abort_handle());
        }
        let outcome = task.await;
        shared.lock().current = None;

        let failure = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(e) if e.is_panic() => Some(panic_message(e.into_panic())),
            // Aborted: only shutdown does that.
            Err(_) => None,
        };
        let Some(failure) = failure else {
            if !root.is_cancelled() {
                info!("Background worker {name} finished");
            }
            break;
        };
        if root.is_cancelled() {
            // A worker that fails while being cancelled is not restarted.
            warn!("Background worker {name} failed while stopping: {failure}");
            break;
        }
        if started.elapsed() >= backoff.reset_after {
            consecutive = 0;
        }
        consecutive += 1;
        let delay = backoff.delay(consecutive);
        {
            let mut s = shared.lock();
            s.failures += 1;
            s.state = WorkerState::BackingOff;
        }
        error!(
            worker = %name,
            "Background worker {name} failed: {failure}. Restarting in {:.1} s",
            delay.as_secs_f64()
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = root.cancelled() => break,
        }
    }
    shared.lock().state = WorkerState::Stopped;
}

/// The text of a panic payload.
pub fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        format!("panicked: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("panicked: {s}")
    } else {
        "panicked".to_string()
    }
}

/// The supervisor's view by name, for tests.
pub fn status_by_name(supervisor: &WorkerSupervisor) -> HashMap<String, WorkerStatus> {
    supervisor
        .status()
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn counter() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

    /// Lets spawned tasks run while virtual time stands still.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let b = Backoff::default();
        let secs: Vec<u64> = (1..=8).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(b.delay(1000), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_worker_is_restarted_after_a_growing_backoff() {
        let calls = counter();
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let sup = WorkerSupervisor::new();
        let (c, s) = (calls.clone(), starts.clone());
        sup.register("flaky", move |_token| {
            let (c, s) = (c.clone(), s.clone());
            async move {
                s.lock().push(Instant::now());
                if c.fetch_add(1, Ordering::SeqCst) < 2 {
                    anyhow::bail!("not yet");
                }
                Ok(())
            }
        });
        let t0 = Instant::now();
        sup.start();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let offsets: Vec<u64> = starts.lock().iter().map(|t| (*t - t0).as_secs()).collect();
        assert_eq!(offsets, vec![0, 1, 3], "1 s, then 2 s");
        let st = status_by_name(&sup);
        assert_eq!(st["flaky"].failures, 2);
        assert_eq!(st["flaky"].state, WorkerState::Stopped, "Ok(()) means finished");
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_worker_is_restarted() {
        let calls = counter();
        let sup = WorkerSupervisor::new();
        let c = calls.clone();
        sup.register("panicky", move |token: CancellationToken| {
            let c = c.clone();
            async move {
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("boom");
                }
                token.cancelled().await;
                Ok(())
            }
        });
        sup.start();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let st = status_by_name(&sup);
        assert_eq!(st["panicky"].failures, 1);
        assert_eq!(st["panicky"].state, WorkerState::Running);
        let report = sup.shutdown(SHUTDOWN_TIMEOUT).await;
        assert_eq!(report.stopped, vec!["panicky".to_string()]);
        assert!(report.abandoned.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_that_finishes_is_not_restarted() {
        let calls = counter();
        let sup = WorkerSupervisor::new();
        let c = calls.clone();
        sup.register("once", move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        sup.start();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(status_by_name(&sup)["once"].state, WorkerState::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_cooperative_workers_and_abandons_the_rest_at_the_deadline() {
        let sup = WorkerSupervisor::new();
        let cooperative_saw_cancel = Arc::new(AtomicU32::new(0));
        let seen = cooperative_saw_cancel.clone();
        sup.register("cooperative", move |token: CancellationToken| {
            let seen = seen.clone();
            async move {
                token.cancelled().await;
                // A little clean-up, well inside the budget.
                tokio::time::sleep(Duration::from_secs(2)).await;
                seen.store(1, Ordering::SeqCst);
                Ok(())
            }
        });
        sup.register("stubborn", |_token| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(())
        });
        sup.start();
        settle().await;
        let t0 = Instant::now();
        let report = sup.shutdown(SHUTDOWN_TIMEOUT).await;
        assert_eq!(report.stopped, vec!["cooperative".to_string()]);
        assert_eq!(report.abandoned, vec!["stubborn".to_string()]);
        assert_eq!(cooperative_saw_cancel.load(Ordering::SeqCst), 1);
        assert_eq!(
            (Instant::now() - t0).as_secs(),
            10,
            "waited the whole budget, no more"
        );
        assert!(
            status_by_name(&sup)
                .values()
                .all(|s| s.state == WorkerState::Stopped)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_backoff_wait() {
        let sup = WorkerSupervisor::with_backoff(Backoff {
            initial: Duration::from_secs(30),
            ..Backoff::default()
        });
        let calls = counter();
        let c = calls.clone();
        sup.register("broken", move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("always")
            }
        });
        sup.start();
        settle().await;
        assert_eq!(status_by_name(&sup)["broken"].state, WorkerState::BackingOff);
        let t0 = Instant::now();
        let report = sup.shutdown(SHUTDOWN_TIMEOUT).await;
        assert_eq!(report.stopped, vec!["broken".to_string()]);
        assert!(Instant::now() - t0 < Duration::from_secs(1));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_healthy_run_resets_the_backoff() {
        let sup = WorkerSupervisor::new();
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let s = starts.clone();
        sup.register("mostly-fine", move |_| {
            let s = s.clone();
            async move {
                let n = {
                    let mut v = s.lock();
                    v.push(Instant::now());
                    v.len()
                };
                // Runs 1 and 2 fail at once; run 3 fails after a healthy two minutes.
                if n == 3 {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
                if n <= 4 {
                    anyhow::bail!("run {n} failed");
                }
                std::future::pending::<()>().await;
                Ok(())
            }
        });
        let t0 = Instant::now();
        sup.start();
        tokio::time::sleep(Duration::from_secs(200)).await;
        let offsets: Vec<u64> = starts.lock().iter().map(|t| (*t - t0).as_secs()).collect();
        // 0 (fail) +1 → 1 (fail) +2 → 3 (healthy 120 s, fail at 123) +1 → 124 (fail) +2 → 126
        assert_eq!(offsets, vec![0, 1, 3, 124, 126]);
    }

    #[tokio::test(start_paused = true)]
    async fn registering_after_start_starts_the_worker() {
        let sup = WorkerSupervisor::new();
        sup.start();
        let calls = counter();
        let c = calls.clone();
        sup.register("late", move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_stopping_token_is_cancelled_by_shutdown() {
        let sup = WorkerSupervisor::new();
        let token = sup.stopping_token();
        sup.start();
        assert!(!token.is_cancelled());
        sup.shutdown(SHUTDOWN_TIMEOUT).await;
        assert!(token.is_cancelled());
    }
}
