//! STUB(4-A): replaced when 4-A lands with the port of `Services/Soulseek/SoulseekLink.cs`.
//! Only the interface is here, as 4-A declares it, for the heart chain (4-D) to wait out an
//! outage.

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};

pub use octo_core::soulseek::soulseek_link::{SoulseekLinkState, SoulseekServerReading};

/// Whether slskd is logged in to Soulseek, for the parts of Octo that should wait for it rather
/// than settle for a lossy copy. An interface so the heart chain and the workers can be tested
/// against a scripted outage.
#[async_trait]
pub trait ISoulseekLink: Send + Sync {
    /// None when slskd did not answer at all. `fresh` skips the short cache.
    async fn read(&self, fresh: bool) -> Option<SoulseekServerReading>;

    /// How long a Soulseek-first download waits for slskd to log back in. Zero is off.
    fn hold_limit(&self) -> TimeDelta;

    fn utc_now(&self) -> DateTime<Utc>;

    /// Returns once slskd is logged in, cannot say, or `deadline_utc` has passed.
    /// True when Soulseek is worth trying; false when the wait ran out with slskd still out.
    async fn wait_for_login(&self, deadline_utc: DateTime<Utc>) -> bool;
}
