//! The clock services read the time from. Not a C# file: where the C# took a
//! `Func<DateTime>` or a `TimeProvider` so tests could fix the time, the Rust takes a
//! [`Clock`].

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};

/// A source of the current UTC time, cheap to clone and share.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>);

impl Clock {
    /// A clock that reads the given function, for tests that move time along.
    pub fn new(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    /// The system clock (`DateTime.UtcNow`, `TimeProvider.System`).
    pub fn system() -> Self {
        Self::new(Utc::now)
    }

    /// A clock stopped at one instant.
    pub fn fixed(at: DateTime<Utc>) -> Self {
        Self::new(move || at)
    }

    /// The current time by this clock.
    pub fn now(&self) -> DateTime<Utc> {
        (self.0)()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Clock").field(&self.now()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[test]
    fn fixed_and_custom_clocks_read_what_they_are_given() {
        let at = DateTime::parse_from_rfc3339("2026-10-04T12:00:00Z")
            .expect("a date")
            .with_timezone(&Utc);
        assert_eq!(Clock::fixed(at).now(), at);

        let seconds = Arc::new(AtomicI64::new(0));
        let ticking = {
            let seconds = seconds.clone();
            Clock::new(move || at + chrono::TimeDelta::seconds(seconds.load(Ordering::SeqCst)))
        };
        seconds.store(90, Ordering::SeqCst);
        assert_eq!(ticking.clone().now(), at + chrono::TimeDelta::seconds(90));

        let before = Utc::now();
        let now = Clock::system().now();
        assert!(now >= before);
    }
}
