//! Port of `Services/Library/GeneratedPlaylistService.cs`: `generated-playlists.json`.
//!
//! Genre and decade mixes built from the listener's own library and served like radio stations
//! (#54): per listener, read-only, under ids that start "og" so they can never be mistaken for a
//! Navidrome playlist. Nothing is written to Navidrome.
//!
//! Which mixes exist is decided from counts refreshed every RefreshHours, with hysteresis so a
//! genre sitting at the threshold does not appear and vanish on alternate days. What is in a mix
//! is a seeded draw per listener and period, so it holds still for the period and every client
//! sees the same tracks, then changes. The rules are
//! `octo_core::library::generated_playlist_service`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::{SingleFlight, dotnet};
use octo_core::json::dom::Node;
use octo_core::library::generated_playlist_service::{
    BLEND_CANDIDATES, FIRST_DECADE, MAX_GENRE_PAGES, POOL_PAGE, StateDocument, UserMixes, apply_hysteresis,
    blend, describe, dotnet_ticks, is_stale, kinds_of, parse_genres, period_index, playlist_id, seed, select,
    user_key,
};
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind};
use octo_core::settings::{GeneratedPlaylistSettings, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub use octo_core::library::generated_playlist_service::GeneratedPlaylist;

use crate::services::framework::{DotnetDictionary, MemoryCache};
use crate::services::last_fm::OperationCanceled;
use crate::services::state_file;
use crate::services::subsonic::SubsonicProxyService;

const FIRST_LIST_WAIT: Duration = Duration::from_millis(1500);
const REFRESH_DEADLINE: Duration = Duration::from_secs(120);
const RETRY_AFTER_FAILURE: TimeDelta = TimeDelta::minutes(5);

/// The listeners' mixes and when each was last counted, under `_lock`.
#[derive(Default)]
struct Mixes {
    users: IndexMap<String, UserMixes>,
    last_attempt: HashMap<String, DateTime<Utc>>,
}

pub struct GeneratedPlaylistService {
    state_path: Option<PathBuf>,
    /// The scope's `SubsonicProxyService`: a background one, with no request behind it.
    proxy: SubsonicProxyService,
    /// `IOptionsMonitor<GeneratedPlaylistSettings>` and `IOptionsMonitor<GenreSettings>`.
    settings: Arc<SettingsStore>,
    refreshes: SingleFlight<String, bool>,
    /// The draws, and the Discovery blend's candidates, until their period ends.
    drawn: MemoryCache<Arc<Vec<Node>>>,
    state: Mutex<Mixes>,
}

impl GeneratedPlaylistService {
    /// Loads the counts from `state_path` (None or blank keeps them in memory only).
    pub fn new(
        state_path: Option<PathBuf>,
        proxy: SubsonicProxyService,
        settings: Arc<SettingsStore>,
    ) -> Self {
        let state_path = state_path.filter(|path| !dotnet::is_blank(&path.to_string_lossy()));
        let service = GeneratedPlaylistService {
            state_path,
            proxy,
            settings,
            refreshes: SingleFlight::new(),
            drawn: MemoryCache::new(256),
            state: Mutex::new(Mixes::default()),
        };
        service.load();
        service
    }

    /// The listener's mixes, refreshing the counts behind them when they are stale. The first
    /// list ever waits a moment for them, so mixes show on a listener's first look rather than
    /// their second; later refreshes run behind the answer.
    pub async fn list(
        self: &Arc<Self>,
        username: &str,
        auth: &IndexMap<String, String>,
    ) -> Vec<GeneratedPlaylist> {
        let settings = self.settings.current().generated_playlists.clone();
        if !settings.enabled || dotnet::is_blank(username) || !(settings.genres || settings.decades) {
            return Vec::new();
        }

        let user = user_key(username);
        let now = Utc::now();
        let (known, due) = {
            let mut state = self.state.lock();
            let current = state.users.get(&user);
            let known = current.is_some();
            let due = current.is_none_or(|current| is_stale(current, &settings, now))
                && state
                    .last_attempt
                    .get(&user)
                    .is_none_or(|attempted| now - *attempted >= RETRY_AFTER_FAILURE);
            if due {
                state.last_attempt.insert(user.clone(), now);
            }
            (known, due)
        };

        if due {
            let service = Arc::clone(self);
            let name = username.to_string();
            let copy = auth.clone();
            let refresh = self.refreshes.run(
                user,
                move |token| async move { service.refresh(&name, &copy, &token).await },
                REFRESH_DEADLINE,
            );
            let name = username.to_string();
            let logged = tokio::spawn(async move {
                if let Err(error) = refresh.await {
                    debug!("Mix refresh failed for {name}: {error:#}");
                }
            });
            if !known {
                let _ = tokio::time::timeout(FIRST_LIST_WAIT, logged).await;
            }
        }
        self.current(username, &settings, Utc::now())
    }

    /// A mix by id, for this listener, as of now. None for anyone else's id.
    pub fn find(&self, username: &str, id: &str) -> Option<GeneratedPlaylist> {
        let settings = self.settings.current().generated_playlists.clone();
        if !settings.enabled || id.is_empty() || !id.starts_with("og") {
            return None;
        }
        self.current(username, &settings, Utc::now())
            .into_iter()
            .find(|mix| mix.id == id)
    }

    fn current(
        &self,
        username: &str,
        settings: &GeneratedPlaylistSettings,
        now_utc: DateTime<Utc>,
    ) -> Vec<GeneratedPlaylist> {
        let user = user_key(username);
        let hours = settings.effective_refresh_hours();
        let period = period_index(now_utc, hours);
        let start = DateTime::UNIX_EPOCH + TimeDelta::hours(period * i64::from(hours));
        let end = start + TimeDelta::hours(i64::from(hours));

        let state = self.state.lock();
        let Some(mixes) = state.users.get(&user) else {
            return Vec::new();
        };
        mixes
            .active
            .iter()
            .filter(|key| {
                (settings.genres && key.starts_with("genre:"))
                    || (settings.decades && key.starts_with("decade:"))
            })
            .map(|key| {
                let (kind, label) = describe(key);
                GeneratedPlaylist {
                    id: playlist_id(username, key),
                    key: key.clone(),
                    name: settings.name(&label),
                    kind,
                    label,
                    owner: username.trim().to_string(),
                    pool_size: mixes.counts.get(key).copied().unwrap_or(0),
                    period_start_utc: start,
                    period_end_utc: end,
                }
            })
            .collect()
    }

    /// Count what the listener's library holds per genre and decade, and decide which mixes they
    /// have. Only a complete count replaces the last one: a Navidrome that stops answering half
    /// way must not make every mix vanish.
    async fn refresh(
        &self,
        username: &str,
        auth: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let settings = self.settings.current().generated_playlists.clone();
        let mut counts: IndexMap<String, i32> = IndexMap::new();

        if settings.genres {
            let Some(genres) = self.count_genres(auth).await else {
                return Ok(false);
            };
            counts.extend(genres);
        }
        if settings.decades {
            let last = Utc::now().year() / 10 * 10;
            let mut decade = FIRST_DECADE;
            while decade <= last {
                if cancellation_token.is_cancelled() {
                    return Err(OperationCanceled.into());
                }
                let extra = [
                    ("size", POOL_PAGE.to_string()),
                    ("fromYear", decade.to_string()),
                    ("toYear", (decade + 9).to_string()),
                ];
                let Some(songs) = self
                    .fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                else {
                    return Ok(false);
                };
                counts.insert(format!("decade:{decade}"), songs.len() as i32);
                decade += 10;
            }
        }

        {
            let mut state = self.state.lock();
            let user = user_key(username);
            let previous = state
                .users
                .get(&user)
                .map(|old| old.active.clone())
                .unwrap_or_default();
            let active = apply_hysteresis(
                &counts,
                &previous,
                settings.effective_create_at(),
                settings.effective_remove_below(),
                settings.effective_max_playlists(),
            );
            state.users.insert(
                user,
                UserMixes {
                    active,
                    counts: counts.clone(),
                    counts_utc: Utc::now(),
                    kinds: kinds_of(&settings),
                },
            );
            self.save_locked(&state);
        }
        info!(
            "Mixes for {username}: {} from {} genres and decades",
            counts.values().filter(|count| **count > 0).count(),
            counts.len()
        );
        Ok(true)
    }

    /// Genres by song count. Spellings that differ only in case are one genre, under the
    /// spelling with the most songs; years and the genre blocklist are never a mix of their own.
    async fn count_genres(&self, auth: &IndexMap<String, String>) -> Option<IndexMap<String, i32>> {
        let result = self
            .proxy
            .relay_safe("rest/getGenres", parameters(auth, &[]))
            .await?;
        if result.body.is_empty() {
            return None;
        }
        let root = Node::parse(&String::from_utf8_lossy(&result.body)).ok()?;
        let response = root.get("subsonic-response")?;
        if response.get("status").and_then(Node::as_str) != Some("ok") {
            return None;
        }
        let blocked = self.settings.current().genre.effective_blocklist();
        let rows = match response.get("genres").and_then(|genres| genres.get("genre")) {
            Some(Node::Array(rows)) => Some(rows.as_slice()),
            _ => None,
        };
        Some(parse_genres(rows, &blocked))
    }

    /// The songs of one mix for this period: the same for every request in the period.
    pub async fn materialize(
        &self,
        username: &str,
        playlist: &GeneratedPlaylist,
        auth: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Vec<Node>> {
        let cache_key = draw_key(username, playlist);
        if let Some(cached) = self.drawn.get(&cache_key) {
            return Ok(cached.as_ref().clone());
        }

        let settings = self.settings.current().generated_playlists.clone();
        let mut pool: Vec<Node> = Vec::new();
        if playlist.kind == "decade" {
            let decade: i32 = playlist
                .key
                .get("decade:".len()..)
                .unwrap_or_default()
                .parse()
                .map_err(|_| {
                    anyhow::anyhow!("The input string '{}' was not in a correct format.", playlist.key)
                })?;
            let extra = [
                ("size", POOL_PAGE.to_string()),
                ("fromYear", decade.to_string()),
                ("toYear", (decade + 9).to_string()),
            ];
            pool.extend(
                self.fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                    .unwrap_or_default(),
            );
        } else {
            for page in 0..MAX_GENRE_PAGES {
                if cancellation_token.is_cancelled() {
                    return Err(OperationCanceled.into());
                }
                let extra = [
                    ("genre", playlist.label.clone()),
                    ("count", POOL_PAGE.to_string()),
                    ("offset", (page * POOL_PAGE).to_string()),
                ];
                let Some(songs) = self
                    .fetch_songs(auth, "rest/getSongsByGenre", "songsByGenre", &extra)
                    .await
                else {
                    break;
                };
                let full = songs.len() as i32 >= POOL_PAGE;
                pool.extend(songs);
                if !full {
                    break;
                }
            }
        }

        let period = period_index(playlist.period_start_utc, settings.effective_refresh_hours());
        let drawn = select(
            &pool,
            settings.effective_track_count(),
            settings.effective_max_per_artist(),
            settings.effective_new_share(),
            settings.effective_new_days(),
            Utc::now(),
            seed(username, &playlist.key, period),
        );
        if !pool.is_empty() {
            self.drawn.set(
                cache_key,
                Arc::new(drawn.clone()),
                1,
                until(playlist.period_end_utc),
            );
        }
        Ok(drawn)
    }

    /// The songs of one mix for this period if they have been drawn already, else None. Never
    /// fetches.
    pub fn drawn(&self, username: &str, playlist: &GeneratedPlaylist) -> Option<Vec<Node>> {
        self.drawn
            .get(&draw_key(username, playlist))
            .map(|cached| cached.as_ref().clone())
    }

    /// Keep MIX_NEW_SHARE percent of Discovery Mix for library songs new to the listener.
    /// Discovery is otherwise all outside the library, so what someone added and never played was
    /// the one thing it could not surface. The station's songs come back as they were when there
    /// is nothing to put in.
    pub async fn blend_into_discovery(
        &self,
        username: &str,
        station: &LastFmRadioStation,
        songs: Vec<Song>,
        auth: &IndexMap<String, String>,
    ) -> Vec<Song> {
        let settings = self.settings.current().generated_playlists.clone();
        let share = settings.effective_new_share();
        if share == 0 || station.kind != LastFmRadioStationKind::Discovery || songs.is_empty() {
            return songs;
        }
        let wanted = (songs.len() as f64 * f64::from(share) / 100.0).round() as i32;
        if wanted == 0 {
            return songs;
        }

        // Only the candidates are cached, and applied to the station as it is now: a track that
        // became a library song since must not be put back as it was.
        let cache_key = format!(
            "blend|{}|{}|{}",
            user_key(username),
            station.id,
            dotnet_ticks(station.changed_utc)
        );
        let candidates = match self.drawn.get(&cache_key) {
            Some(candidates) => candidates,
            None => {
                let extra = [("size", BLEND_CANDIDATES.to_string())];
                let Some(fetched) = self
                    .fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                else {
                    return songs;
                };
                let fetched = Arc::new(fetched);
                let now = Utc::now();
                let expires = if station.valid_until_utc > now {
                    station.valid_until_utc
                } else {
                    now + TimeDelta::hours(1)
                };
                self.drawn.set(cache_key, Arc::clone(&fetched), 1, until(expires));
                fetched
            }
        };
        match blend(
            &songs,
            &candidates,
            wanted,
            settings.effective_new_days(),
            Utc::now(),
        ) {
            Some(blended) => blended,
            None => songs,
        }
    }

    /// The songs of one Subsonic call, or None when it did not answer: an empty list is a real
    /// answer and None is not, and only the first may replace what is known.
    async fn fetch_songs(
        &self,
        auth: &IndexMap<String, String>,
        endpoint: &str,
        container: &str,
        extra: &[(&str, String)],
    ) -> Option<Vec<Node>> {
        let extra: Vec<(&str, &str)> = extra.iter().map(|(key, value)| (*key, value.as_str())).collect();
        let Some(result) = self.proxy.relay_safe(endpoint, parameters(auth, &extra)).await else {
            debug!("Mix lookup {endpoint} failed: no answer");
            return None;
        };
        if result.body.is_empty() {
            return None;
        }
        let root = match Node::parse(&String::from_utf8_lossy(&result.body)) {
            Ok(root) => root,
            Err(error) => {
                debug!("Mix lookup {endpoint} failed: {error}");
                return None;
            }
        };
        let response = root.get("subsonic-response")?;
        if response.get("status").and_then(Node::as_str) != Some("ok") {
            return None;
        }
        Some(
            match response.get(container).and_then(|songs| songs.get("song")) {
                Some(Node::Array(songs)) => songs.iter().filter(|song| song.is_object()).cloned().collect(),
                _ => Vec::new(),
            },
        )
    }

    fn load(&self) {
        let Some(path) = self.state_path.as_ref().filter(|path| path.is_file()) else {
            return;
        };
        let read = state_file::read_all_text(path)
            .map_err(|error| error.to_string())
            .and_then(|text| serde_json::from_str::<StateDocument>(&text).map_err(|error| error.to_string()));
        match read {
            Ok(document) => {
                let Some(users) = document.users else { return };
                let mut state = self.state.lock();
                for (user, mixes) in users {
                    state.users.insert(user, mixes);
                }
            }
            Err(message) => {
                // Only counts: the next list rebuilds them, so a bad file is set aside, not fatal.
                warn!("Mix state could not be read ({message}); it will be rebuilt");
                let mut corrupt = path.as_os_str().to_owned();
                corrupt.push(format!(".corrupt-{}", dotnet_ticks(Utc::now())));
                // Best effort.
                let _ = std::fs::rename(path, PathBuf::from(corrupt));
            }
        }
    }

    fn save_locked(&self, state: &Mixes) {
        let Some(path) = &self.state_path else { return };
        let document = StateDocument {
            users: Some(state.users.clone()),
        };
        if let Err(error) = state_file::save_atomic(path, &octo_core::json::to_string(&document)) {
            warn!("Mix state could not be written: {error}");
        }
    }
}

fn draw_key(username: &str, playlist: &GeneratedPlaylist) -> String {
    format!(
        "{}|{}|{}",
        user_key(username),
        playlist.id,
        dotnet_ticks(playlist.period_start_utc)
    )
}

/// An absolute expiry as the time left until it.
fn until(expires: DateTime<Utc>) -> Duration {
    (expires - Utc::now()).to_std().unwrap_or(Duration::ZERO)
}

/// The caller's own parameters, whatever auth they carry (a token, a password or an API key),
/// as the sync catalog does, minus anything that named the playlist being read.
/// In the order the C# `Dictionary` enumerated them, freed slots reused, which is the order the
/// query is written in.
fn parameters(auth: &IndexMap<String, String>, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut parameters: DotnetDictionary<String> = DotnetDictionary::new();
    for (key, value) in auth {
        parameters.set(key.clone(), value.clone());
    }
    parameters.remove("id");
    parameters.remove("playlistId");
    for (key, value) in extra {
        parameters.set((*key).to_string(), (*value).to_string());
    }
    parameters.set("f".to_string(), "json".to_string());
    parameters
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
#[path = "generated_playlist_service_tests.rs"]
mod tests;
