//! Port of `Services/LastFm/LastFmRadioStreamSessionStore.cs`.
//!
//! Bounded in-memory authorization bridge between an authenticated Subsonic station-list request
//! and the credential-free streamUrl a radio client opens. Tokens disappear on restart and never
//! expose usernames or Navidrome secrets.

use std::path::PathBuf;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::models::radio::LastFmRadioTrack;
use parking_lot::Mutex;

use crate::services::framework::DotnetDictionary;

/// One complete MP3 segment in the radio cache, ready to play: the file, the station track,
/// its place in the snapshot, and the cache key it was stored under.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRadioTrack {
    pub path: PathBuf,
    pub track: LastFmRadioTrack,
    pub index: usize,
    pub cache_key: String,
}

impl PreparedRadioTrack {
    pub fn new(
        path: impl Into<PathBuf>,
        track: LastFmRadioTrack,
        index: usize,
        cache_key: impl Into<String>,
    ) -> Self {
        PreparedRadioTrack {
            path: path.into(),
            track,
            index,
            cache_key: cache_key.into(),
        }
    }
}

/// A station a listener may stream, behind an opaque token. `authentication` holds only the
/// Subsonic credential parameters (`StringComparer.OrdinalIgnoreCase` keys).
#[derive(Debug, Clone, PartialEq)]
pub struct LastFmRadioStreamSession {
    pub token: String,
    pub username: String,
    pub station_id: String,
    pub authentication: IndexMap<String, String>,
    pub expires_utc: DateTime<Utc>,
    pub ready_pool: Option<Vec<PreparedRadioTrack>>,
}

impl LastFmRadioStreamSession {
    pub fn new(
        token: impl Into<String>,
        username: impl Into<String>,
        station_id: impl Into<String>,
        authentication: IndexMap<String, String>,
        expires_utc: DateTime<Utc>,
    ) -> Self {
        LastFmRadioStreamSession {
            token: token.into(),
            username: username.into(),
            station_id: station_id.into(),
            authentication,
            expires_utc,
            ready_pool: None,
        }
    }

    /// `Authentication[name]`, ignoring case.
    pub fn authentication_value(&self, name: &str) -> Option<&str> {
        self.authentication
            .iter()
            .find(|(key, _)| dotnet::eq_ignore_case(key, name))
            .map(|(_, value)| value.as_str())
    }
}

pub const MAXIMUM_SESSIONS: usize = 1024;
const LIFETIME: TimeDelta = TimeDelta::hours(12);

/// What the stream later relays to Navidrome as this listener (a library song, its scrobble).
/// An API key sign-in has only apiKey, so without it those relays had no credentials at all.
const AUTHENTICATION_KEYS: [&str; 7] = ["u", "p", "t", "s", "apiKey", "v", "c"];

#[derive(Default)]
pub struct LastFmRadioStreamSessionStore {
    sessions: Mutex<DotnetDictionary<LastFmRadioStreamSession>>,
}

impl LastFmRadioStreamSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new token for the station, carrying only the credential parameters of the request.
    pub fn issue<'a, I>(
        &self,
        username: &str,
        station_id: &str,
        request_parameters: I,
        now_utc: Option<DateTime<Utc>>,
    ) -> String
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let now = now_utc.unwrap_or_else(Utc::now);
        let mut auth: IndexMap<String, String> = IndexMap::new();
        for (key, value) in request_parameters {
            if !AUTHENTICATION_KEYS
                .iter()
                .any(|allowed| dotnet::eq_ignore_case(allowed, key))
            {
                continue;
            }
            // ToDictionary(OrdinalIgnoreCase) threw on a duplicate; the parameters are a
            // dictionary already, so the first spelling stands.
            if !auth.keys().any(|existing| dotnet::eq_ignore_case(existing, key)) {
                auth.insert(key.to_string(), value.to_string());
            }
        }
        let token = hex::encode(rand::random::<[u8; 24]>());
        let session =
            LastFmRadioStreamSession::new(token.clone(), username, station_id, auth, now + LIFETIME);

        let mut sessions = self.sessions.lock();
        prune_locked(&mut sessions, now);
        while sessions.len() >= MAXIMUM_SESSIONS {
            let Some(oldest) = sessions
                .values()
                .fold(None::<&LastFmRadioStreamSession>, |best, item| match best {
                    Some(best) if best.expires_utc <= item.expires_utc => Some(best),
                    _ => Some(item),
                })
                .map(|item| item.token.clone())
            else {
                break;
            };
            sessions.remove(&oldest);
        }
        sessions.set(token.clone(), session);
        token
    }

    pub fn get(&self, token: &str, now_utc: Option<DateTime<Utc>>) -> Option<LastFmRadioStreamSession> {
        let now = now_utc.unwrap_or_else(Utc::now);
        let mut sessions = self.sessions.lock();
        prune_locked(&mut sessions, now);
        sessions.get(token).cloned()
    }

    pub fn attach_ready_pool(
        &self,
        token: &str,
        ready_pool: &[PreparedRadioTrack],
        now_utc: Option<DateTime<Utc>>,
    ) -> bool {
        let now = now_utc.unwrap_or_else(Utc::now);
        let mut sessions = self.sessions.lock();
        prune_locked(&mut sessions, now);
        match sessions.get_mut(token) {
            Some(session) => {
                session.ready_pool = Some(ready_pool.to_vec());
                true
            }
            None => false,
        }
    }

    pub fn consume_ready_track(&self, token: &str, cache_key: &str) {
        let mut sessions = self.sessions.lock();
        let Some(session) = sessions.get_mut(token) else {
            return;
        };
        let mut pool = session.ready_pool.take().unwrap_or_default();
        if let Some(index) = pool.iter().position(|item| item.cache_key == cache_key) {
            pool.remove(index);
        }
        session.ready_pool = Some(pool);
    }

    pub fn append_ready_track(&self, token: &str, track: PreparedRadioTrack, maximum: usize) {
        let mut sessions = self.sessions.lock();
        let Some(session) = sessions.get_mut(token) else {
            return;
        };
        let mut pool = session.ready_pool.take().unwrap_or_default();
        if pool.iter().all(|item| item.cache_key != track.cache_key) {
            pool.push(track);
        }
        pool.truncate(maximum);
        session.ready_pool = Some(pool);
    }

    pub fn remove(&self, token: &str) {
        self.sessions.lock().remove(token);
    }

    pub fn count(&self) -> usize {
        self.sessions.lock().len()
    }
}

fn prune_locked(sessions: &mut DotnetDictionary<LastFmRadioStreamSession>, now_utc: DateTime<Utc>) {
    let expired: Vec<String> = sessions
        .values()
        .filter(|session| session.expires_utc <= now_utc)
        .map(|session| session.token.clone())
        .collect();
    for token in expired {
        sessions.remove(&token);
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::services::last_fm::last_fm_radio_stream_service::READY_POOL_SIZE;

    // LastFmRadioCoreTests.StreamSessions_AreOpaqueScopedExpiringAndBounded
    #[test]
    fn stream_sessions_are_opaque_scoped_expiring_and_bounded() {
        let store = LastFmRadioStreamSessionStore::new();
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 1, 0, 0).unwrap();
        let token = store.issue(
            "alice",
            "station-one",
            [
                ("u", "alice"),
                ("t", "secret-token"),
                ("s", "salt"),
                ("id", "must-not-be-copied"),
                ("f", "json"),
            ],
            Some(now),
        );
        assert!(!token.contains("alice"));
        assert!(!token.contains("secret"));
        let session = store
            .get(&token, Some(now + TimeDelta::hours(1)))
            .expect("a session");
        assert_eq!(session.station_id, "station-one");
        assert_eq!(session.authentication_value("t"), Some("secret-token"));
        assert!(!session.authentication.contains_key("id"));
        let pool: Vec<PreparedRadioTrack> = (0..READY_POOL_SIZE)
            .map(|index| {
                PreparedRadioTrack::new(
                    format!("/tmp/ready-{index}.mp3"),
                    LastFmRadioTrack {
                        artist: "Artist".into(),
                        title: format!("Title {index}"),
                        ..Default::default()
                    },
                    index,
                    format!("key-{index}"),
                )
            })
            .collect();
        assert!(store.attach_ready_pool(&token, &pool, Some(now + TimeDelta::hours(1))));
        assert_eq!(
            store
                .get(&token, Some(now + TimeDelta::hours(1)))
                .unwrap()
                .ready_pool
                .unwrap(),
            pool
        );
        store.consume_ready_track(&token, "key-0");
        let keys = |store: &LastFmRadioStreamSessionStore| -> Vec<String> {
            store
                .get(&token, Some(now + TimeDelta::hours(1)))
                .unwrap()
                .ready_pool
                .unwrap()
                .into_iter()
                .map(|item| item.cache_key)
                .collect()
        };
        assert_eq!(keys(&store), ["key-1", "key-2"]);
        let replacement = PreparedRadioTrack::new(
            "/tmp/ready-3.mp3",
            LastFmRadioTrack {
                artist: "Artist".into(),
                title: "Title 3".into(),
                ..Default::default()
            },
            3,
            "key-3",
        );
        store.append_ready_track(&token, replacement, READY_POOL_SIZE);
        assert_eq!(keys(&store), ["key-1", "key-2", "key-3"]);
        assert!(store.get(&token, Some(now + TimeDelta::hours(13))).is_none());

        for index in 0..MAXIMUM_SESSIONS as i64 + 20 {
            store.issue(
                "alice",
                &format!("station-{index}"),
                std::iter::empty(),
                Some(now + TimeDelta::seconds(index)),
            );
        }
        assert_eq!(store.count(), MAXIMUM_SESSIONS);
    }

    // LastFmRadioCoreTests.RepeatedValueReader_PreservesQueryAndFormBatches: a station list's
    // batch of ids reaches the radio whether the client sent it in the query or in a form body.
    #[test]
    fn repeated_value_reader_preserves_query_and_form_batches() {
        use axum::http::{HeaderMap, HeaderValue, Method, header};

        use crate::services::subsonic::subsonic_proxy_service::IncomingRequest;

        let body = bytes::Bytes::from_static(b"id=f1&id=f2");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        let request = IncomingRequest::new(Method::POST, headers, Some("id=q1&id=q2".into()), body);
        assert_eq!(request.parameter_values("id"), ["q1", "q2", "f1", "f2"]);
    }

    // RequestIdentityTests.RadioStreamSession_KeepsTheApiKey_ForTheRelaysItMakesLater
    #[test]
    fn radio_stream_session_keeps_the_api_key_for_the_relays_it_makes_later() {
        let store = LastFmRadioStreamSessionStore::new();
        let token = store.issue(
            "bob",
            "station",
            [
                ("apiKey", "bob-key"),
                ("v", "1.16.1"),
                ("c", "x"),
                ("id", "not-auth"),
            ],
            None,
        );

        let session = store.get(&token, None).expect("a session");
        assert_eq!(session.authentication_value("apiKey"), Some("bob-key"));
        assert!(!session.authentication.contains_key("id"));
    }

    /// Rust-only: tokens are 48 lower-case hex characters, keys match ignoring case, and the
    /// oldest session is the one a full store lets go.
    #[test]
    fn tokens_are_hex_and_the_oldest_session_goes_first() {
        let store = LastFmRadioStreamSessionStore::new();
        let now = Utc::now();
        let first = store.issue("a", "s", [("U", "a"), ("APIKEY", "k")], Some(now));
        assert_eq!(first.len(), 48);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let session = store.get(&first, Some(now)).unwrap();
        assert_eq!(session.authentication_value("u"), Some("a"));
        assert_eq!(session.authentication_value("apiKey"), Some("k"));
        for index in 1..MAXIMUM_SESSIONS as i64 {
            store.issue(
                "a",
                "s",
                std::iter::empty(),
                Some(now + TimeDelta::seconds(index)),
            );
        }
        store.issue("a", "s", std::iter::empty(), Some(now + TimeDelta::seconds(5000)));
        assert!(store.get(&first, Some(now)).is_none());
        store.remove("missing");
        assert!(!store.attach_ready_pool("missing", &[], None));
    }
}
