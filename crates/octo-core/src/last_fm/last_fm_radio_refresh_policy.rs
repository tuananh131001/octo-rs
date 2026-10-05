//! Port of `Services/LastFm/LastFmRadioRefreshPolicy.cs`: when a listener's stations are due a
//! rebuild.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use sha2::{Digest, Sha256};

use crate::common::dotnet;
use crate::models::radio::LastFmRadioUserState;
use crate::settings::LastFmSettings;

const FAILURE_RETRY_DELAY: TimeDelta = TimeDelta::minutes(15);

/// No successful refresh yet, or the last one is older than the refresh interval.
pub fn is_stale(user: &LastFmRadioUserState, settings: &LastFmSettings, now_utc: DateTime<Utc>) -> bool {
    match user.last_refresh_success_utc {
        None => true,
        Some(success) => {
            success < now_utc - TimeDelta::hours(i64::from(settings.effective_refresh_interval_hours()))
        }
    }
}

/// After a recorded play: no stations yet, stale, enough new plays, or the play that makes the
/// profile learned.
pub fn should_refresh_after_play(
    user: &LastFmRadioUserState,
    settings: &LastFmSettings,
    now_utc: DateTime<Utc>,
) -> bool {
    let learned_count = user.plays.iter().filter(|play| play.learned_signal).count() as i32;
    user.stations.is_empty()
        || is_stale(user, settings, now_utc)
        || user.new_plays_since_refresh >= 3.max(settings.effective_minimum_plays() / 2)
        || learned_count == settings.effective_minimum_plays()
}

/// The minute scan: stale and not refreshing, and a failed refresh is retried only after a
/// quarter of an hour.
pub fn should_schedule_periodic_refresh(
    user: &LastFmRadioUserState,
    settings: &LastFmSettings,
    now_utc: DateTime<Utc>,
) -> bool {
    if user.refreshing || !is_stale(user, settings, now_utc) {
        return false;
    }
    user.last_refresh_error.is_none()
        || user
            .last_refresh_attempt_utc
            .is_none_or(|attempt| attempt <= now_utc - FAILURE_RETRY_DELAY)
}

/// A deterministic 100–499 ms per listener, so a restart does not refresh every profile at once.
pub fn startup_jitter(username: &str) -> Duration {
    let hash = Sha256::digest(dotnet::to_lower_invariant(username.trim()).as_bytes());
    let value = u16::from_le_bytes([hash[0], hash[1]]);
    Duration::from_millis(100 + u64::from(value % 400))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::radio::{LastFmRadioPlay, LastFmRadioStation};

    // LastFmRadioRefreshQueueTests.Policy_CoversThresholdNewPlayStalenessAndDeterministicStartupJitter
    #[test]
    fn policy_covers_threshold_new_play_staleness_and_deterministic_startup_jitter() {
        let settings = LastFmSettings {
            minimum_plays: 10,
            refresh_interval_hours: 12,
            ..Default::default()
        };
        let now = Utc::now();
        let mut user = LastFmRadioUserState {
            plays: (0..9)
                .map(|index| LastFmRadioPlay {
                    artist: "A".into(),
                    title: format!("T{index}"),
                    learned_signal: true,
                    ..Default::default()
                })
                .collect(),
            stations: vec![LastFmRadioStation::default()],
            last_refresh_success_utc: Some(now),
            new_plays_since_refresh: 4,
            ..Default::default()
        };
        assert!(!should_refresh_after_play(&user, &settings, now));
        user.new_plays_since_refresh = 5;
        assert!(should_refresh_after_play(&user, &settings, now));
        user.new_plays_since_refresh = 0;
        user.plays.push(LastFmRadioPlay {
            artist: "A".into(),
            title: "threshold".into(),
            learned_signal: true,
            ..Default::default()
        });
        assert!(should_refresh_after_play(&user, &settings, now));
        user.plays.pop();
        user.last_refresh_success_utc = Some(now - TimeDelta::hours(13));
        assert!(is_stale(&user, &settings, now));
        assert!(should_schedule_periodic_refresh(&user, &settings, now));
        user.refreshing = true;
        assert!(!should_schedule_periodic_refresh(&user, &settings, now));
        user.refreshing = false;
        user.last_refresh_error = Some("provider unavailable".into());
        user.last_refresh_attempt_utc = Some(now - TimeDelta::minutes(5));
        assert!(!should_schedule_periodic_refresh(&user, &settings, now));
        user.last_refresh_attempt_utc = Some(now - TimeDelta::minutes(16));
        assert!(should_schedule_periodic_refresh(&user, &settings, now));
        let jitter = startup_jitter("Alice");
        assert_eq!(jitter, startup_jitter("alice"));
        assert!((100..=499).contains(&jitter.as_millis()));
    }

    /// What .NET 9 computed for these names.
    #[test]
    fn startup_jitter_is_what_dotnet_computed() {
        assert_eq!(startup_jitter("Alice"), Duration::from_millis(239));
        assert_eq!(startup_jitter(" bob "), Duration::from_millis(421));
    }
}
