//! `Octo.Models.Settings.LastFmSettings`, `LastFmUserSession` and `DiscoveryStationSettings`
//! (the `LastFm` section).

use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::text::{IgnoreCaseSet, eq_ignore_case, lower_invariant, utf16_len};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LastFmSettings {
    /// Last.fm API key for fetching similar tracks
    pub api_key: String,

    /// The shared secret that comes with the API key, from the same Last.fm API account
    /// page. Only scrobbling needs it: Last.fm wants every call made for a listener signed
    /// with it. The admin API never hands it back.
    /// Environment variable: LASTFM_API_SECRET
    pub api_secret: String,

    /// Send plays to each connected listener's Last.fm. Outside songs always go when this
    /// is on; library songs go too while `scrobble_library_plays` is on.
    pub scrobble_external_plays: bool,

    /// Send library plays to Last.fm as well as outside ones. On by default: Navidrome scrobbles
    /// a listener only once they link Last.fm in Navidrome's own web settings, which Octo's apps
    /// never show, so without this most plays never reach Last.fm. Turn it off for an Octo whose
    /// Navidrome is linked to the same Last.fm account, or each library play counts twice.
    pub scrobble_library_plays: bool,

    /// Navidrome username to that listener's Last.fm session, written by Connect on the
    /// dashboard. A session key is a password for scrobbling as that person, so the admin
    /// API only ever shows it masked.
    /// Environment variable form: LASTFM__USERSESSIONS__alice__SESSIONKEY=...
    ///
    /// The C# dictionary compares keys with OrdinalIgnoreCase; look sessions up with
    /// [`LastFmSettings::session_for`].
    pub user_sessions: IndexMap<String, LastFmUserSession>,

    /// Enable/disable the radio feature
    pub enable_radio: bool,

    /// Maximum tracks in song-seeded queues and reusable station snapshots.
    pub radio_track_count: i32,

    /// Cache duration for Last.fm lookups in hours
    pub radio_cache_duration_hours: i32,

    /// Automatically learn per-user stations from completed plays.
    pub enable_personalized_stations: bool,

    /// Build the listener's own mix. When they have not been learned from yet this is
    /// Starter Radio, which is the only station a new user gets, so turning it off can
    /// leave them with nothing at all until plays accumulate.
    /// Environment variable: LASTFM_ENABLE_YOUR_MIX
    pub enable_your_mix: bool,

    /// Build the Discovery Mix, seeded from the listener's top tags.
    /// Environment variable: LASTFM_ENABLE_DISCOVERY_MIX
    pub enable_discovery_mix: bool,

    /// How many per-artist radios to build, most-played first. 0 disables them.
    /// Environment variable: LASTFM_ARTIST_STATION_COUNT
    pub artist_station_count: i32,

    /// How many per-genre radios to build, from the listener's top tags. 0 disables them.
    /// Environment variable: LASTFM_GENRE_STATION_COUNT
    pub genre_station_count: i32,

    /// Expose administrator-pinned Last.fm tag stations.
    pub enable_discovery_stations: bool,

    /// Publish Octo stations as normal read-only playlists.
    pub expose_radio_as_playlists: bool,

    /// Publish Octo stations through Subsonic internet radio.
    pub expose_radio_as_streams: bool,

    /// Continuous internet-radio MP3 bitrate.
    pub radio_stream_bitrate_kbps: i32,

    /// Embed the current station track as opt-in ICY stream metadata.
    pub enable_icy_metadata: bool,

    /// How long a station-list request waits for a cold station's first track before
    /// answering without it. Production keeps going in the background and the station
    /// appears on the client's next refresh. 0 waits for the starter however long it
    /// takes, which is the original behaviour.
    pub starter_publish_timeout_seconds: i32,

    /// EBU R128 integrated loudness every radio track is brought to before it joins the
    /// stream, in LUFS. Tracks arrive from local FLAC and from YouTube previews at
    /// levels many LU apart; a static gain per track (with a true-peak limiter at
    /// -1 dBTP) makes the station one level. 0 disables normalisation.
    pub radio_loudness_target_lufs: i32,

    pub history_retention_days: i32,
    pub discovery_percent: i32,
    pub refresh_interval_hours: i32,
    pub minimum_plays: i32,
    pub discovery_stations: Vec<DiscoveryStationSettings>,
}

impl Default for LastFmSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_secret: String::new(),
            scrobble_external_plays: true,
            scrobble_library_plays: true,
            user_sessions: IndexMap::new(),
            enable_radio: true,
            radio_track_count: 50,
            radio_cache_duration_hours: 24,
            enable_personalized_stations: true,
            enable_your_mix: true,
            enable_discovery_mix: true,
            artist_station_count: 2,
            genre_station_count: 3,
            enable_discovery_stations: true,
            expose_radio_as_playlists: true,
            expose_radio_as_streams: true,
            radio_stream_bitrate_kbps: 192,
            enable_icy_metadata: true,
            starter_publish_timeout_seconds: 8,
            radio_loudness_target_lufs: -16,
            history_retention_days: 90,
            discovery_percent: 35,
            refresh_interval_hours: 12,
            minimum_plays: 10,
            discovery_stations: Vec::new(),
        }
    }
}

impl LastFmSettings {
    /// This listener's session, or None when they have not connected Last.fm.
    /// Matched without regard to case, as Navidrome matches usernames.
    pub fn session_for(&self, username: &str) -> Option<&LastFmUserSession> {
        if username.trim().is_empty() {
            return None;
        }
        let name = username.trim();
        let session = self.user_sessions.get(name).or_else(|| {
            self.user_sessions
                .iter()
                .find(|(k, _)| eq_ignore_case(k, name))
                .map(|(_, v)| v)
        });
        session.filter(|s| !s.session_key.trim().is_empty())
    }

    pub fn effective_history_retention_days(&self) -> i32 {
        self.history_retention_days.clamp(7, 365)
    }

    pub fn effective_radio_track_count(&self) -> i32 {
        self.radio_track_count.clamp(10, 100)
    }

    pub fn effective_radio_cache_duration_hours(&self) -> i32 {
        self.radio_cache_duration_hours.clamp(1, 168)
    }

    pub fn effective_discovery_percent(&self) -> i32 {
        self.discovery_percent.clamp(0, 100)
    }

    pub fn effective_refresh_interval_hours(&self) -> i32 {
        self.refresh_interval_hours.clamp(1, 168)
    }

    pub fn effective_minimum_plays(&self) -> i32 {
        self.minimum_plays.clamp(3, 100)
    }

    pub fn effective_artist_station_count(&self) -> i32 {
        self.artist_station_count.clamp(0, 5)
    }

    pub fn effective_genre_station_count(&self) -> i32 {
        self.genre_station_count.clamp(0, 5)
    }

    /// Whether the settings ask for nothing to be built: dynamic stations are on, every
    /// individual kind is off, and no pinned station is contributing either.
    ///
    /// Narrow on purpose. The refresh worker treats an empty build as a provider failure
    /// and keeps the last good snapshot rather than replacing it with nothing, which is
    /// the behaviour that stops a bad Last.fm response from wiping a listener's stations.
    /// Switching every kind off is the one empty build that is a choice rather than a
    /// failure, so only that case is excused; every configuration that predates the
    /// per-type settings keeps the old guard exactly.
    pub fn stations_explicitly_empty(&self) -> bool {
        self.enable_personalized_stations
            && !self.enable_your_mix
            && !self.enable_discovery_mix
            && self.effective_artist_station_count() == 0
            && self.effective_genre_station_count() == 0
            && !(self.enable_discovery_stations
                && self
                    .effective_discovery_stations()
                    .iter()
                    .any(|station| station.enabled))
    }

    pub fn effective_radio_loudness_target(&self) -> Option<f64> {
        if self.radio_loudness_target_lufs == 0 {
            None
        } else {
            Some(f64::from(self.radio_loudness_target_lufs.clamp(-23, -9)))
        }
    }

    pub fn effective_starter_publish_timeout(&self) -> Option<Duration> {
        if self.starter_publish_timeout_seconds <= 0 {
            None
        } else {
            Some(Duration::from_secs(
                self.starter_publish_timeout_seconds.clamp(1, 300) as u64,
            ))
        }
    }

    pub fn effective_radio_stream_bitrate_kbps(&self) -> i32 {
        match self.radio_stream_bitrate_kbps {
            ..=96 => 96,
            97..=128 => 128,
            129..=192 => 192,
            193..=256 => 256,
            _ => 320,
        }
    }

    pub fn effective_discovery_stations(&self) -> Vec<DiscoveryStationSettings> {
        let mut seen_ids = IgnoreCaseSet::new();
        let mut seen_names = IgnoreCaseSet::new();
        let mut result = Vec::new();

        for source in self.discovery_stations.iter().take(12) {
            let name = source.name.trim().to_string();
            let mut seen_tags = IgnoreCaseSet::new();
            let tags: Vec<String> = source
                .tags
                .iter()
                .map(|t| DiscoveryStationSettings::normalize_tag(t))
                .filter(|tag| (1..=80).contains(&utf16_len(tag)))
                .filter(|tag| seen_tags.insert(tag.clone()))
                .take(5)
                .collect();
            let name_len = utf16_len(&name);
            if name_len == 0 || name_len > 100 || tags.is_empty() || !seen_names.insert(name.clone()) {
                continue;
            }

            let mut id = DiscoveryStationSettings::normalize_id(&source.id);
            if id.is_empty() {
                id = DiscoveryStationSettings::deterministic_id(&name, &tags);
            }
            if !seen_ids.insert(id.clone()) {
                continue;
            }

            result.push(DiscoveryStationSettings {
                id,
                name,
                enabled: source.enabled,
                tags,
            });
        }
        result
    }
}

/// One listener's link to their Last.fm account. Last.fm session keys do not
/// expire; one stops working only when the listener revokes Octo on last.fm.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LastFmUserSession {
    pub session_key: String,

    /// The Last.fm account the key belongs to, shown on the dashboard.
    pub last_fm_user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct DiscoveryStationSettings {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub tags: Vec<String>,
}

impl Default for DiscoveryStationSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            tags: Vec::new(),
        }
    }
}

impl DiscoveryStationSettings {
    /// Trimmed, lowercased, and runs of spaces collapsed to one. Only the space character
    /// separates words here (`Split(' ')`); a tab inside a tag is kept.
    pub fn normalize_tag(value: &str) -> String {
        lower_invariant(value.trim())
            .split(' ')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// ASCII letters and digits only, the first 32 of them, lowercased.
    pub fn normalize_id(value: &str) -> String {
        if value.trim().is_empty() {
            return String::new();
        }
        value
            .trim()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(32)
            .collect::<String>()
            .to_ascii_lowercase()
    }

    /// The first 12 bytes of SHA-256 over `lower(trim(name)) + "|" + join("|", tags)`, as 24
    /// lowercase hex characters.
    pub fn deterministic_id<S: AsRef<str>>(name: &str, tags: &[S]) -> String {
        let joined = tags.iter().map(AsRef::as_ref).collect::<Vec<_>>().join("|");
        let seed = format!("{}|{}", lower_invariant(name.trim()), joined);
        let hash = Sha256::digest(seed.as_bytes());
        hex::encode(&hash[..12])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(id: &str, name: &str, tags: &[&str]) -> DiscoveryStationSettings {
        DiscoveryStationSettings {
            id: id.into(),
            name: name.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        }
    }

    // LastFmRadioCoreTests.DiscoverySettings_NormalizeClampAndKeepStableIds
    #[test]
    fn discovery_settings_normalize_clamp_and_keep_stable_ids() {
        let settings = LastFmSettings {
            history_retention_days: 1,
            radio_track_count: 500,
            discovery_percent: -4,
            discovery_stations: vec![station(
                " Keep-ME! ",
                " Electronic ",
                &[" IDM ", "idm", "Electronic", "Ambient", "Techno", "House"],
            )],
            ..Default::default()
        };
        let stations = settings.effective_discovery_stations();
        assert_eq!(stations.len(), 1);
        let station = &stations[0];
        assert_eq!(station.id, "keepme");
        assert_eq!(station.tags, ["idm", "electronic", "ambient", "techno", "house"]);
        assert_eq!(settings.effective_history_retention_days(), 7);
        assert_eq!(settings.effective_radio_track_count(), 100);
        assert_eq!(settings.effective_discovery_percent(), 0);
    }

    // LastFmRadioCoreTests.StationCounts_AreClampedAtReadTime
    #[test]
    fn station_counts_are_clamped_at_read_time() {
        for (configured, effective) in [(-3, 0), (0, 0), (2, 2), (99, 5)] {
            let settings = LastFmSettings {
                artist_station_count: configured,
                genre_station_count: configured,
                ..Default::default()
            };
            assert_eq!(
                settings.effective_artist_station_count(),
                effective,
                "{configured}"
            );
            assert_eq!(
                settings.effective_genre_station_count(),
                effective,
                "{configured}"
            );
        }
    }

    // LastFmRadioCoreTests.StationsExplicitlyEmpty_ExcusesOnlyAnAllKindsOffConfiguration
    #[test]
    fn stations_explicitly_empty_excuses_only_an_all_kinds_off_configuration() {
        let all_off = LastFmSettings {
            enable_personalized_stations: true,
            enable_your_mix: false,
            enable_discovery_mix: false,
            artist_station_count: 0,
            genre_station_count: 0,
            enable_discovery_stations: false,
            ..Default::default()
        };
        assert!(all_off.stations_explicitly_empty());

        assert!(!LastFmSettings::default().stations_explicitly_empty());

        let only_genre = LastFmSettings {
            genre_station_count: 1,
            ..all_off.clone()
        };
        assert!(!only_genre.stations_explicitly_empty());

        // A pinned station still counts, so an empty build is still a provider failure.
        let pinned_survives = LastFmSettings {
            enable_discovery_stations: true,
            discovery_stations: vec![station("rock", "Rock", &["rock"])],
            ..all_off.clone()
        };
        assert!(!pinned_survives.stations_explicitly_empty());

        // Every configuration that predates the per-type settings keeps the old guard.
        let personalized_off = LastFmSettings {
            enable_personalized_stations: false,
            ..Default::default()
        };
        assert!(!personalized_off.stations_explicitly_empty());
    }

    // LastFmRadioCoreTests.StreamBitrate_UsesSupportedMp3Qualities
    #[test]
    fn stream_bitrate_uses_supported_mp3_qualities() {
        for (configured, effective) in [(1, 96), (120, 128), (192, 192), (220, 256), (999, 320)] {
            let settings = LastFmSettings {
                radio_stream_bitrate_kbps: configured,
                ..Default::default()
            };
            assert_eq!(
                settings.effective_radio_stream_bitrate_kbps(),
                effective,
                "{configured}"
            );
        }
    }

    #[test]
    fn deterministic_id_is_24_hex_characters_of_sha256() {
        let id = DiscoveryStationSettings::deterministic_id(" Rock ", &["rock"]);
        assert_eq!(id.len(), 24);
        let full = hex::encode(Sha256::digest(b"rock|rock"));
        assert_eq!(id, full[..24]);
    }

    #[test]
    fn session_for_ignores_case_and_blank_keys() {
        let mut settings = LastFmSettings::default();
        settings.user_sessions.insert(
            "Alice".into(),
            LastFmUserSession {
                session_key: "sk".into(),
                last_fm_user: "a".into(),
            },
        );
        settings
            .user_sessions
            .insert("bob".into(), LastFmUserSession::default());
        assert_eq!(
            settings.session_for(" alice ").map(|s| s.session_key.as_str()),
            Some("sk")
        );
        assert!(settings.session_for("bob").is_none());
        assert!(settings.session_for("  ").is_none());
    }

    #[test]
    fn loudness_and_starter_timeout_treat_zero_as_off() {
        let s = LastFmSettings::default();
        assert_eq!(s.effective_radio_loudness_target(), Some(-16.0));
        assert_eq!(
            s.effective_starter_publish_timeout(),
            Some(Duration::from_secs(8))
        );
        let off = LastFmSettings {
            radio_loudness_target_lufs: 0,
            starter_publish_timeout_seconds: 0,
            ..Default::default()
        };
        assert_eq!(off.effective_radio_loudness_target(), None);
        assert_eq!(off.effective_starter_publish_timeout(), None);
        let loud = LastFmSettings {
            radio_loudness_target_lufs: -2,
            starter_publish_timeout_seconds: 1000,
            ..Default::default()
        };
        assert_eq!(loud.effective_radio_loudness_target(), Some(-9.0));
        assert_eq!(
            loud.effective_starter_publish_timeout(),
            Some(Duration::from_secs(300))
        );
    }
}
