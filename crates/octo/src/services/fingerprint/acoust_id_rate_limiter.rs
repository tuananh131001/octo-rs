//! Port of `Services/Fingerprint/AcoustIdRateLimiter.cs`.

use std::future::Future;
use std::time::Duration;

use crate::services::framework::{RateLimitLease, SlidingWindowRateLimiter, SlidingWindowRateLimiterOptions};

tokio::task_local! {
    /// `AsyncLocal<bool> BackgroundFlow`: set for the work run by
    /// [`AcoustIdRateLimiter::in_background`]. Unlike an `AsyncLocal`, it does not reach a task
    /// that work spawns; the lookups are awaited inline, so nothing relied on that.
    static BACKGROUND_FLOW: bool;
}

/// Keeps Octo inside AcoustID's published budget of 3 requests per second.
///
/// The same trap as Deezer, and it bites harder here: over-budget is not reliably a 429,
/// it is an error envelope in a 200 body. That parses as "no match", and this feature reads
/// "no match" as "accept the file", so exceeding the budget would silently turn
/// verification OFF rather than merely slow it down.
///
/// One budget with a background lane. A download's lookup sits between a finished transfer and
/// the library and waits in the queue. The library sweep (#72) never waits: it takes a permit
/// only when one is free and no download is queued (OldestFirst refuses a non-queuing request
/// while anyone waits), so it can never take a queue slot a download needed.
pub struct AcoustIdRateLimiter {
    limiter: SlidingWindowRateLimiter,
}

impl AcoustIdRateLimiter {
    /// Named HttpClient that carries the limiting handler. A caller that resolves
    /// any other client bypasses the budget entirely.
    pub const CLIENT_NAME: &'static str = "acoustid";

    const PERMITS_PER_SECOND: u32 = 3;

    pub fn new() -> Self {
        Self {
            limiter: SlidingWindowRateLimiter::new(SlidingWindowRateLimiterOptions {
                permit_limit: Self::PERMITS_PER_SECOND,
                window: Duration::from_secs(1),
                segments_per_window: 3,
                // Must be set explicitly: it defaults to 0, which makes AcquireAsync return a
                // NON-acquired lease immediately instead of waiting. A dropped lookup here is an
                // accepted file, so dropping is the one thing this must not do by default.
                // Bounded by the client's 10s timeout: 3/s drains 12 in four seconds.
                queue_limit: 12,
            }),
        }
    }

    /// Lookups started inside `work` use the background lane. The flag ends when `work` does
    /// and never reaches the caller.
    pub async fn in_background<T>(work: impl Future<Output = T>) -> T {
        BACKGROUND_FLOW.scope(true, work).await
    }

    /// Whether the current work runs in the background lane (`internal static bool InBackground`).
    pub fn in_background_now() -> bool {
        BACKGROUND_FLOW.try_with(|flag| *flag).unwrap_or(false)
    }

    pub async fn acquire(&self) -> RateLimitLease {
        if Self::in_background_now() {
            self.limiter.attempt_acquire()
        } else {
            self.limiter.acquire().await
        }
    }
}

impl Default for AcoustIdRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    /// The sweep never queues: with the budget spent it is refused at once, while a download's
    /// lookup waits for the next permit.
    #[tokio::test(start_paused = true)]
    async fn the_background_lane_never_waits() {
        let limiter = AcoustIdRateLimiter::new();
        for _ in 0..3 {
            assert!(limiter.acquire().await.is_acquired());
        }

        let refused = AcoustIdRateLimiter::in_background(async { limiter.acquire().now_or_never() }).await;
        assert!(!refused.expect("answered at once").is_acquired());
        assert!(!AcoustIdRateLimiter::in_background_now());

        assert!(limiter.acquire().await.is_acquired());
    }
}
