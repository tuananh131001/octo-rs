//! Port of `Services/Metadata/DeezerRateLimiter.cs`.

use std::time::Duration;

use crate::services::framework::{RateLimitLease, SlidingWindowRateLimiter, SlidingWindowRateLimiterOptions};

/// Keeps Octo inside Deezer's public-API budget of roughly 50 requests per 5 seconds.
///
/// Exceeding it does not produce a 429. Deezer answers HTTP 200 with an error envelope,
/// which used to parse as a valid-but-empty payload and get cached, so going over budget
/// was silently destructive rather than merely slow (issue #8).
///
/// Two lanes, one budget. Interactive work (a search the user is waiting on, cover art a
/// client is rendering) must not queue behind background cache warming. The permit counts
/// are chosen to SUM below the ceiling: separate limiters each get their own window, so
/// two generous lanes would admit their total rather than capping it.
pub struct DeezerRateLimiter {
    interactive: SlidingWindowRateLimiter,
    background: SlidingWindowRateLimiter,
}

impl DeezerRateLimiter {
    /// Named HttpClient that carries the limiting handler. Both the metadata
    /// service and the cover-art lookup must resolve this name or they bypass the budget.
    pub const CLIENT_NAME: &'static str = "deezer";

    const WINDOW: Duration = Duration::from_secs(5);

    // 30 + 10 = 40 of Deezer's ~50, leaving headroom for retries elsewhere and for the
    // fact that the ceiling is approximate rather than published as a contract.
    const INTERACTIVE_PERMITS: u32 = 30;
    const BACKGROUND_PERMITS: u32 = 10;

    pub fn new() -> Self {
        // Queue depths are bounded by the 8s HttpClient timeout, which now covers waiting for
        // a permit as well as the call itself. Interactive drains at 6/s, so 32 queued is a
        // ~5s worst case and stays inside it; a deeper queue would just manufacture timeouts.
        Self {
            interactive: Self::build(Self::INTERACTIVE_PERMITS, 32),
            background: Self::build(Self::BACKGROUND_PERMITS, 32),
        }
    }

    fn build(permits: u32, queue_limit: u32) -> SlidingWindowRateLimiter {
        SlidingWindowRateLimiter::new(SlidingWindowRateLimiterOptions {
            permit_limit: permits,
            window: Self::WINDOW,
            // One-second granularity, so permits free up smoothly instead of all at once
            // at the end of each window.
            segments_per_window: 5,
            // Must be set explicitly: it defaults to 0, which makes AcquireAsync return a
            // NON-acquired lease immediately instead of waiting. Every over-budget call
            // would then be dropped rather than delayed.
            queue_limit,
        })
    }

    pub async fn acquire(&self, background: bool) -> RateLimitLease {
        if background {
            self.background.acquire().await
        } else {
            self.interactive.acquire().await
        }
    }
}

impl Default for DeezerRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}
