//! The pure half of `Services/Soulseek/SoulseekLink.cs`: slskd's login state, one reading of
//! it, and the dashboard's words for it. The link itself (the cached read and the wait) is
//! `octo::services::soulseek::soulseek_link`.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Whether slskd is logged in to Soulseek, as far as one reading can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoulseekLinkState {
    LoggedIn,
    NotLoggedIn,
    Unknown,
}

impl SoulseekLinkState {
    /// The member's name, as C#'s `ToString()` wrote it.
    pub fn name(self) -> &'static str {
        match self {
            SoulseekLinkState::LoggedIn => "LoggedIn",
            SoulseekLinkState::NotLoggedIn => "NotLoggedIn",
            SoulseekLinkState::Unknown => "Unknown",
        }
    }
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

    /// A reading that says nothing: a shape without the flags. Never holds anything back.
    pub fn unknown() -> Self {
        Self::new(SoulseekLinkState::Unknown, None, None, None)
    }
}

/// What a request refused during an outage says.
pub const OFFLINE_TEXT: &str = "Soulseek is not connected (slskd is not logged in), so nothing changed. Octo tries again when it is back.";

/// Short enough that a heart just after slskd logs back in is not held for nothing, long
/// enough that forty held songs share one request.
pub const CACHE_FOR: Duration = Duration::from_secs(10);

/// slskd itself retries about every five minutes, so looking more often finds nothing new.
pub const POLL_EVERY: Duration = Duration::from_secs(30);

/// The dashboard's line for slskd: (ok, warning, detail). Not logged in is a warning, not a
/// failure: slskd is up and logs back in by itself.
pub fn describe(reading: Option<&SoulseekServerReading>, hold_hours: i32) -> (bool, bool, String) {
    match reading {
        None => (false, false, "unreachable / auth failed".to_string()),
        Some(r) if r.link == SoulseekLinkState::LoggedIn => (
            true,
            false,
            match r.username.as_deref().filter(|name| !name.is_empty()) {
                Some(name) => format!("logged in to Soulseek as {name}"),
                None => "logged in to Soulseek".to_string(),
            },
        ),
        Some(r) if r.link == SoulseekLinkState::NotLoggedIn => (true, true, outage_detail(r, hold_hours)),
        Some(_) => (true, false, "reachable".to_string()),
    }
}

/// The words for an outage: what slskd says, how long downloads wait, and slskd's next try.
pub fn outage_detail(reading: &SoulseekServerReading, hold_hours: i32) -> String {
    let mut text = format!(
        "Not connected to Soulseek (slskd says {}). ",
        reading.state.as_deref().unwrap_or("not logged in")
    );
    if hold_hours > 0 {
        text.push_str(&format!(
            "Downloads wait up to {hold_hours} {} for it, then use the next source.",
            if hold_hours == 1 { "hour" } else { "hours" }
        ));
    } else {
        text.push_str("Downloads use the next source.");
    }
    if let Some(next) = reading.next_attempt_utc {
        text.push_str(&format!(" slskd tries again at {} UTC.", next.format("%H:%M")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn not_logged_in() -> SoulseekServerReading {
        SoulseekServerReading::new(
            SoulseekLinkState::NotLoggedIn,
            Some("Disconnecting".into()),
            Some("winters27".into()),
            Some(
                Utc.with_ymd_and_hms(2026, 10, 3, 5, 48, 13)
                    .single()
                    .expect("a date"),
            ),
        )
    }

    /// SoulseekOutageTests.TheDashboardWarnsWhenSlskdIsUpButNotLoggedIn
    #[test]
    fn the_dashboard_warns_when_slskd_is_up_but_not_logged_in() {
        let reading = not_logged_in();
        let (ok, warning, detail) = describe(Some(&reading), 6);
        assert!(ok);
        assert!(warning);
        assert!(detail.contains("Not connected to Soulseek"), "{detail}");
        assert!(detail.contains("slskd says Disconnecting"), "{detail}");
        assert!(detail.contains("6 hours"), "{detail}");
        assert!(detail.contains("05:48 UTC"), "{detail}");

        assert!(
            describe(Some(&reading), 0)
                .2
                .contains("Downloads use the next source.")
        );
        assert!(describe(Some(&reading), 1).2.contains("1 hour for it"));
    }

    /// SoulseekOutageTests.TheDashboardLinesForTheOtherStates (the logged-in line with the
    /// reading parsed from slskd's answer is in `soulseek_client`'s tests).
    #[test]
    fn the_dashboard_lines_for_the_other_states() {
        assert_eq!(
            describe(None, 6),
            (false, false, "unreachable / auth failed".to_string())
        );
        let logged_in = SoulseekServerReading::new(
            SoulseekLinkState::LoggedIn,
            Some("Connected, LoggedIn".into()),
            Some("winters27".into()),
            None,
        );
        assert_eq!(
            describe(Some(&logged_in), 6),
            (true, false, "logged in to Soulseek as winters27".to_string())
        );
        let anonymous =
            SoulseekServerReading::new(SoulseekLinkState::LoggedIn, None, Some(String::new()), None);
        assert_eq!(describe(Some(&anonymous), 6).2, "logged in to Soulseek");
        assert_eq!(
            describe(Some(&SoulseekServerReading::unknown()), 6),
            (true, false, "reachable".to_string())
        );
    }

    /// SoulseekOutageTests.NoDashLikeCharactersInTheWords
    #[test]
    fn no_dash_like_characters_in_the_words() {
        let detail = describe(Some(&not_logged_in()), 6).2;
        assert!(!detail.contains('\u{2014}'));
        assert!(!detail.contains('\u{2013}'));
        assert!(!detail.contains(" - "));
        assert!(!OFFLINE_TEXT.contains('\u{2014}'));
    }

    #[test]
    fn an_outage_without_a_state_or_next_try_says_so_plainly() {
        let reading = SoulseekServerReading::new(SoulseekLinkState::NotLoggedIn, None, None, None);
        assert_eq!(
            outage_detail(&reading, 2),
            "Not connected to Soulseek (slskd says not logged in). Downloads wait up to 2 hours for it, then use the next source."
        );
        assert_eq!(SoulseekLinkState::NotLoggedIn.name(), "NotLoggedIn");
    }
}
