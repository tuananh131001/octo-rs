//! STUB(4-A): replaced when 4-A lands with the pure half of `Services/Soulseek/SoulseekLink.cs`.
//! Only slskd's login state and one reading of it are here, as 4-A declares them, for the heart
//! chain (4-D) to wait out an outage against `ISoulseekLink`.

use chrono::{DateTime, Utc};

/// Whether slskd is logged in to Soulseek, as far as one reading can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoulseekLinkState {
    LoggedIn,
    NotLoggedIn,
    Unknown,
}

/// One reading of slskd's application state. State is slskd's own words, such as
/// "Disconnecting" or "Connected, LoggedIn". NextAttemptUtc is when slskd next tries to connect,
/// when it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulseekServerReading {
    pub link: SoulseekLinkState,
    pub state: Option<String>,
    pub username: Option<String>,
    pub next_attempt_utc: Option<DateTime<Utc>>,
}

impl SoulseekServerReading {
    pub fn new(
        link: SoulseekLinkState,
        state: Option<String>,
        username: Option<String>,
        next_attempt_utc: Option<DateTime<Utc>>,
    ) -> Self {
        SoulseekServerReading {
            link,
            state,
            username,
            next_attempt_utc,
        }
    }
}
