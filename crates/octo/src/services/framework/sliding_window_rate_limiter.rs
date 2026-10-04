//! `System.Threading.RateLimiting.SlidingWindowRateLimiter`, as the Deezer and AcoustID limiters
//! configure it: `QueueProcessingOrder.OldestFirst`, `AutoReplenishment = true`, one permit per
//! request. Not a C# file of Octo's; the framework type the two limiters were built on.
//!
//! The window is cut into segments. A permit taken during a segment comes back when the window
//! has slid all the way round to that segment again, so a burst spends the budget and it
//! returns a segment at a time, not when a lease is disposed (leases carry nothing back). A
//! request that finds no permit waits in a bounded first-in-first-out queue; when the queue is
//! full it gets a lease that was not acquired, at once.
//!
//! .NET moves the window with a timer. Here the window moves lazily: every acquire first
//! replays the segment boundaries that have passed since the limiter was built (on the tokio
//! clock, so `tokio::time::pause` drives it in tests), and a queued request sleeps until the
//! next boundary and replays it. The boundaries fall where the timer would have fired them.

use std::collections::VecDeque;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// The options the C# set; the rest were the defaults Octo relied on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlidingWindowRateLimiterOptions {
    pub permit_limit: u32,
    pub window: Duration,
    pub segments_per_window: u32,
    pub queue_limit: u32,
}

/// The outcome of one acquire. A sliding window's lease gives nothing back when it is
/// dropped, so this is only the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitLease {
    acquired: bool,
}

impl RateLimitLease {
    pub fn is_acquired(&self) -> bool {
        self.acquired
    }
}

struct Waiter {
    tx: oneshot::Sender<()>,
}

struct State {
    /// Permits free now.
    permit_count: u32,
    /// Permits taken during each segment, given back when the window reaches it again.
    requests_per_segment: Vec<u32>,
    current_segment: usize,
    queue: VecDeque<Waiter>,
    /// Segment boundaries replayed so far.
    ticks: u64,
}

pub struct SlidingWindowRateLimiter {
    options: SlidingWindowRateLimiterOptions,
    segment: Duration,
    origin: Instant,
    state: Mutex<State>,
}

impl SlidingWindowRateLimiter {
    pub fn new(options: SlidingWindowRateLimiterOptions) -> Self {
        assert!(
            options.permit_limit > 0 && options.segments_per_window > 0 && !options.window.is_zero(),
            "a sliding window needs permits, segments and a length"
        );
        Self {
            segment: options.window / options.segments_per_window,
            origin: Instant::now(),
            state: Mutex::new(State {
                permit_count: options.permit_limit,
                requests_per_segment: vec![0; options.segments_per_window as usize],
                current_segment: 0,
                queue: VecDeque::new(),
                ticks: 0,
            }),
            options,
        }
    }

    /// `AttemptAcquire(1)`: a permit now or a lease that was not acquired, never a wait. With
    /// OldestFirst, a request that is waiting in the queue goes first, so this fails while
    /// anyone waits.
    pub fn attempt_acquire(&self) -> RateLimitLease {
        let mut state = self.state.lock();
        self.replenish(&mut state);
        RateLimitLease {
            acquired: Self::try_lease(&mut state),
        }
    }

    /// `AcquireAsync(1)`: a permit now, or a place in the queue until one frees up. A full
    /// queue answers at once with a lease that was not acquired. Dropping the future gives up
    /// the place in the queue, as cancelling the token did.
    pub async fn acquire(&self) -> RateLimitLease {
        let mut rx = {
            let mut state = self.state.lock();
            self.replenish(&mut state);
            if Self::try_lease(&mut state) {
                return RateLimitLease { acquired: true };
            }
            if state.queue.len() as u32 >= self.options.queue_limit {
                return RateLimitLease { acquired: false };
            }
            let (tx, rx) = oneshot::channel();
            state.queue.push_back(Waiter { tx });
            rx
        };

        loop {
            let next = {
                let state = self.state.lock();
                self.boundary(state.ticks + 1)
            };
            tokio::select! {
                biased;
                granted = &mut rx => return RateLimitLease { acquired: granted.is_ok() },
                () = tokio::time::sleep_until(next) => {
                    let mut state = self.state.lock();
                    self.replenish(&mut state);
                }
            }
        }
    }

    fn boundary(&self, tick: u64) -> Instant {
        let nanos = self.segment.as_nanos().saturating_mul(u128::from(tick));
        self.origin + Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// Replays every segment boundary that has passed.
    fn replenish(&self, state: &mut State) {
        // Waiters that gave up leave the queue, as a cancelled request did.
        state.queue.retain(|waiter| !waiter.tx.is_closed());
        let elapsed = Instant::now().saturating_duration_since(self.origin);
        let due = u64::try_from(elapsed.as_nanos() / self.segment.as_nanos()).unwrap_or(u64::MAX);
        while state.ticks < due {
            if state.queue.is_empty() && state.permit_count == self.options.permit_limit {
                // Every segment is empty and nobody waits: the remaining boundaries change
                // nothing.
                state.ticks = due;
                break;
            }
            state.ticks += 1;
            self.rotate(state);
        }
    }

    /// One boundary: the window slides a segment, the permits taken in the segment it now
    /// reuses come back, and queued requests are served oldest first.
    fn rotate(&self, state: &mut State) {
        let segments = state.requests_per_segment.len();
        state.current_segment = (state.current_segment + 1) % segments;
        let current = state.current_segment;
        state.permit_count += std::mem::take(&mut state.requests_per_segment[current]);

        while state.permit_count >= 1 {
            let Some(waiter) = state.queue.pop_front() else {
                break;
            };
            if waiter.tx.send(()).is_ok() {
                state.permit_count -= 1;
                state.requests_per_segment[current] += 1;
            }
        }
    }

    fn try_lease(state: &mut State) -> bool {
        if state.permit_count >= 1 && state.queue.is_empty() {
            let current = state.current_segment;
            state.requests_per_segment[current] += 1;
            state.permit_count -= 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    fn limiter(permits: u32, queue: u32) -> SlidingWindowRateLimiter {
        SlidingWindowRateLimiter::new(SlidingWindowRateLimiterOptions {
            permit_limit: permits,
            window: Duration::from_secs(5),
            segments_per_window: 5,
            queue_limit: queue,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn permits_come_back_when_the_window_has_slid_past_them() {
        let limiter = limiter(3, 4);
        for _ in 0..3 {
            assert!(limiter.acquire().now_or_never().expect("immediate").is_acquired());
        }
        assert!(!limiter.attempt_acquire().is_acquired());

        // Taken in segment 0; the window reaches segment 0 again after five boundaries.
        tokio::time::advance(Duration::from_millis(4_900)).await;
        assert!(!limiter.attempt_acquire().is_acquired());
        tokio::time::advance(Duration::from_millis(200)).await;
        assert!(limiter.attempt_acquire().is_acquired());
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_request_waits_and_a_full_queue_is_refused_at_once() {
        let limiter = limiter(1, 1);
        assert!(limiter.acquire().await.is_acquired());

        let start = Instant::now();
        let queued = limiter.acquire();
        tokio::pin!(queued);
        assert!(futures::poll!(&mut queued).is_pending());
        // The queue holds one, so the next is turned away without waiting.
        assert!(!limiter.acquire().now_or_never().expect("immediate").is_acquired());
        // And a non-queuing attempt never jumps the queue.
        assert!(!limiter.attempt_acquire().is_acquired());

        assert!(queued.await.is_acquired());
        assert_eq!(Instant::now() - start, Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_that_gives_up_frees_its_place() {
        let limiter = limiter(1, 1);
        assert!(limiter.acquire().await.is_acquired());
        {
            let queued = limiter.acquire();
            tokio::pin!(queued);
            assert!(futures::poll!(&mut queued).is_pending());
        }
        let again = limiter.acquire();
        tokio::pin!(again);
        assert!(futures::poll!(&mut again).is_pending());
        assert!(again.await.is_acquired());
    }
}
