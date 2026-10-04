//! Port of `Services/LastFm/LastFmScrobbleService.cs`. The records and the request signature are
//! `octo_core::last_fm::last_fm_scrobble_service`.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use indexmap::map::Entry;
use octo_core::common::{Clock, dotnet};
use octo_core::json::dom::Node;
use octo_core::last_fm::last_fm_scrobble_service::sign;
use octo_core::last_fm::{
    LastFmCredentialCheck, LastFmScrobbleException, LastFmScrobbleUser, LastFmSentPlay, LastFmTrack,
};
use octo_core::settings::{
    JsonObject, LastFmSettings, LastFmUserSession, SettingsFileWriter, SettingsStore, SettingsWriteError,
};
use parking_lot::Mutex;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::net_parse::parse_long;
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;
use crate::services::http_client_factory::{connect_failure_message, timeout_message};

/// What `FinishConnectAsync` threw that its caller told apart: a refusal to show as it is, or
/// settings.json that could not be written (corrupt, locked, read-only, or a full disk).
#[derive(Debug, thiserror::Error)]
pub enum LastFmConnectError {
    #[error(transparent)]
    Refused(#[from] LastFmScrobbleException),
    #[error(transparent)]
    Settings(#[from] SettingsWriteError),
}

/// The waits the C# exposed as internal properties so tests could shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrobbleTuning {
    /// First wait after a failed batch; each further failure doubles it.
    pub retry_delay: TimeDelta,
    /// How long every call stops after Last.fm says Octo is calling too often.
    pub rate_limit_pause: TimeDelta,
    /// How long a session Last.fm refused rests before it is tried once more.
    pub refusal_grace: TimeDelta,
}

impl Default for ScrobbleTuning {
    fn default() -> Self {
        Self {
            retry_delay: TimeDelta::seconds(30),
            rate_limit_pause: TimeDelta::minutes(5),
            refusal_grace: TimeDelta::hours(1),
        }
    }
}

/// A wait on the service's clock: resolves once that much time has passed by it.
pub type Sleep = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// `TimeProvider`: the clock every pause and wait is measured on. Tests move it by hand.
#[derive(Clone)]
pub struct ScrobbleTime {
    pub clock: Clock,
    pub sleep: Sleep,
}

impl ScrobbleTime {
    /// `TimeProvider.System`.
    pub fn system() -> Self {
        Self {
            clock: Clock::system(),
            sleep: Arc::new(|wait| Box::pin(tokio::time::sleep(wait))),
        }
    }
}

/// A `Dictionary` or `ConcurrentDictionary` built with `StringComparer.OrdinalIgnoreCase`: a key
/// keeps the spelling it was first added with.
struct IgnoreCaseMap<V> {
    entries: IndexMap<String, (String, V)>,
}

impl<V> Default for IgnoreCaseMap<V> {
    fn default() -> Self {
        Self {
            entries: IndexMap::new(),
        }
    }
}

impl<V> IgnoreCaseMap<V> {
    fn get(&self, key: &str) -> Option<&V> {
        self.entries
            .get(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, value)| value)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.entries
            .get_mut(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, value)| value)
    }

    /// `map[key] = value`.
    fn insert(&mut self, key: &str, value: V) {
        match self.entries.entry(dotnet::ordinal_ignore_case_key(key)) {
            Entry::Occupied(mut entry) => entry.get_mut().1 = value,
            Entry::Vacant(entry) => {
                entry.insert((key.to_string(), value));
            }
        }
    }

    fn remove(&mut self, key: &str) -> Option<V> {
        self.entries
            .shift_remove(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, value)| value)
    }

    fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.values().map(|(key, _)| key.as_str())
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.entries.values().map(|(key, value)| (key.as_str(), value))
    }
}

#[derive(Debug, Clone)]
struct PendingScrobble {
    /// The C# compared plays by reference; this is that identity.
    id: u64,
    track: LastFmTrack,
    played_at_utc: DateTime<Utc>,
    chosen_by_user: bool,
    attempts: i32,
}

/// What `_gate` guarded.
struct Gate {
    queues: IgnoreCaseMap<Vec<PendingScrobble>>,
    retry_at: IgnoreCaseMap<DateTime<Utc>>,
    paused_until: DateTime<Utc>,
    rate_limit_strikes: i32,
    draining: bool,
}

impl Gate {
    /// The next batch that may go now, or how long until one may.
    fn next_batch(&self, now: DateTime<Utc>) -> (String, Vec<PendingScrobble>, TimeDelta) {
        if self.queues.is_empty() {
            return (String::new(), Vec::new(), TimeDelta::zero());
        }
        if now < self.paused_until {
            return (String::new(), Vec::new(), self.paused_until - now);
        }
        let mut soonest = DateTime::<Utc>::MAX_UTC;
        for (user, queue) in self.queues.iter() {
            let at = self
                .retry_at
                .get(user)
                .copied()
                .unwrap_or(DateTime::<Utc>::MIN_UTC);
            if at <= now {
                return (
                    user.to_string(),
                    queue
                        .iter()
                        .take(LastFmScrobbleService::MAX_BATCH)
                        .cloned()
                        .collect(),
                    TimeDelta::zero(),
                );
            }
            if at < soonest {
                soonest = at;
            }
        }
        (String::new(), Vec::new(), soonest - now)
    }

    /// Removes these plays from the user's queue.
    fn forget(&mut self, user: &str, plays: &[PendingScrobble]) {
        let Some(queue) = self.queues.get_mut(user) else {
            return;
        };
        queue.retain(|queued| !plays.iter().any(|play| play.id == queued.id));
        if !queue.is_empty() {
            return;
        }
        self.queues.remove(user);
        self.retry_at.remove(user);
    }

    /// Stops every call for a while, longer each time Last.fm says so again.
    fn pause(&mut self, now: DateTime<Utc>, rate_limit_pause: TimeDelta) {
        self.rate_limit_strikes += 1;
        let factor = 1i32 << (self.rate_limit_strikes - 1).min(10);
        let pause = (rate_limit_pause * factor).min(LastFmScrobbleService::LONGEST_PAUSE);
        self.paused_until = now + pause;
    }
}

/// One answer from Last.fm.
struct Reply {
    /// The answer, always a JSON object.
    body: Option<Value>,
    error: i32,
    message: String,
    /// Worth sending again later: Last.fm unreachable, a server error, or its own "try again
    /// later" (11, 16).
    retryable: bool,
}

impl Reply {
    fn ok(&self) -> bool {
        self.body.is_some()
    }
}

/// Scrobbles plays to each listener's own Last.fm, and runs the dashboard's Connect flow that
/// links a Navidrome user to a Last.fm account.
///
/// Outside songs always come through here, since Navidrome has never heard of them. Library
/// songs come too unless the admin leaves them to Navidrome
/// ([`takes_library_plays`](Self::takes_library_plays)), for a Navidrome linked to Last.fm
/// itself. Nothing waits on Last.fm: a play is queued and the client has its answer straight
/// away. The queue is in memory and bounded, like the ListenBrainz path it sits beside, so an
/// outage costs at most the plays still waiting when Octo restarts.
pub struct LastFmScrobbleService {
    http: reqwest::Client,
    api_url: String,
    /// `IOptionsMonitor<LastFmSettings>`: read at every use.
    settings: Arc<SettingsStore>,
    settings_file: Arc<SettingsFileWriter>,
    tuning: ScrobbleTuning,
    time: ScrobbleTime,

    gate: Mutex<Gate>,
    // Cuts the drain's wait short when a play arrives, so one listener resting for an hour
    // does not hold back everyone else's.
    wake: Notify,
    now_playing_in_flight: AtomicUsize,
    next_play: AtomicU64,

    // A session Last.fm refused, or one disconnected on the dashboard. Settings reload a moment
    // after the file changes, and one set in the environment is not in the file at all, so this
    // is what stops the key being used again in the meantime.
    revoked_keys: Mutex<HashSet<String>>,
    // When Last.fm first refused a session with error 9. The session rests until the grace is
    // over, its plays kept and still queued; only a second refusal after that removes it from
    // settings.json and drops them, since one error 9 has been known to be Last.fm's hiccup
    // rather than the listener revoking Octo.
    refused_at: Mutex<HashMap<String, DateTime<Utc>>>,
    pending_approvals: Mutex<IgnoreCaseMap<(String, DateTime<Utc>)>>,
    // A session Finish has just written. settings.json reaches the running settings a moment
    // after the write, and until then the listener would read as not connected, so the session
    // is used from here until the reloaded settings carry the same key.
    just_saved: Mutex<IgnoreCaseMap<LastFmUserSession>>,
    last_sent: Mutex<IgnoreCaseMap<LastFmSentPlay>>,
    notices: Mutex<IgnoreCaseMap<String>>,
}

impl LastFmScrobbleService {
    pub const CLIENT_NAME: &'static str = "lastfm-scrobble";
    pub const API_URL: &'static str = "https://ws.audioscrobbler.com/2.0/";
    pub const AUTH_URL: &'static str = "https://www.last.fm/api/auth/";

    /// The named client's timeout. Scrobbles of outside songs go out in the background, never
    /// inside a client's request, but a hung call would still hold up every play queued behind it.
    pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    /// The most plays Last.fm takes in one track.scrobble call.
    pub const MAX_BATCH: usize = 50;
    /// Plays kept per listener while Last.fm is unreachable. Past this the oldest go.
    pub const MAX_QUEUED_PER_USER: usize = 500;
    /// Tries per play before it is given up on.
    pub const MAX_ATTEMPTS: i32 = 5;

    pub const ERROR_INVALID_SESSION: i32 = 9;
    pub const ERROR_TOKEN_NOT_AUTHORIZED: i32 = 14;
    pub const ERROR_TOKEN_EXPIRED: i32 = 15;
    pub const ERROR_RATE_LIMITED: i32 = 29;

    // Last.fm's auth tokens last an hour. A little less, so Finish never races the expiry.
    const TOKEN_LIFETIME: TimeDelta = TimeDelta::minutes(55);
    const LONGEST_RETRY_WAIT: TimeDelta = TimeDelta::minutes(30);
    const LONGEST_PAUSE: TimeDelta = TimeDelta::hours(1);

    /// Last.fm ignores a play older than two weeks, so one is not queued at all.
    pub const OLDEST_PLAY: TimeDelta = TimeDelta::days(14);

    /// The production service: the named client (10 s, no decompression), Last.fm's API, the
    /// C# waits and the system clock.
    pub fn new(settings: Arc<SettingsStore>, settings_file: Arc<SettingsFileWriter>) -> Self {
        let http = client_builder()
            .timeout(Self::CLIENT_TIMEOUT)
            .build()
            .expect("the Last.fm scrobble client builds");
        Self::with_parts(
            http,
            Self::API_URL,
            settings,
            settings_file,
            ScrobbleTuning::default(),
            ScrobbleTime::system(),
        )
    }

    /// Every part given: tests point `api_url` at a fake Last.fm, shorten the waits and move
    /// the clock by hand.
    pub fn with_parts(
        http: reqwest::Client,
        api_url: &str,
        settings: Arc<SettingsStore>,
        settings_file: Arc<SettingsFileWriter>,
        tuning: ScrobbleTuning,
        time: ScrobbleTime,
    ) -> Self {
        Self {
            http,
            api_url: api_url.to_string(),
            settings,
            settings_file,
            tuning,
            time,
            gate: Mutex::new(Gate {
                queues: IgnoreCaseMap::default(),
                retry_at: IgnoreCaseMap::default(),
                paused_until: DateTime::<Utc>::MIN_UTC,
                rate_limit_strikes: 0,
                draining: false,
            }),
            wake: Notify::new(),
            now_playing_in_flight: AtomicUsize::new(0),
            next_play: AtomicU64::new(0),
            revoked_keys: Mutex::new(HashSet::new()),
            refused_at: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(IgnoreCaseMap::default()),
            just_saved: Mutex::new(IgnoreCaseMap::default()),
            last_sent: Mutex::new(IgnoreCaseMap::default()),
            notices: Mutex::new(IgnoreCaseMap::default()),
        }
    }

    pub fn tuning(&self) -> ScrobbleTuning {
        self.tuning
    }

    fn now(&self) -> DateTime<Utc> {
        self.time.clock.now()
    }

    fn last_fm(&self) -> LastFmSettings {
        self.settings.current().last_fm.clone()
    }

    /// Plays queued or being sent, and Now Playing calls still out. Tests wait on it.
    pub fn outstanding(&self) -> usize {
        let queued: usize = self.gate.lock().queues.iter().map(|(_, queue)| queue.len()).sum();
        queued + self.now_playing_in_flight.load(Ordering::SeqCst)
    }

    /// True when the API key and shared secret are both saved, which Connect needs.
    pub fn is_ready(&self) -> bool {
        is_ready_with(&self.last_fm())
    }

    /// True when this listener's plays would be sent to Last.fm. A session resting
    /// after one refusal still counts: its plays wait for it.
    pub fn is_enabled_for(&self, username: &str) -> bool {
        let settings = self.last_fm();
        settings.scrobble_external_plays
            && is_ready_with(&settings)
            && self.saved_session(&settings, username).is_some()
    }

    /// True when library plays go to Last.fm from here too, not only outside ones.
    pub fn takes_library_plays(&self) -> bool {
        self.settings.current().last_fm.scrobble_library_plays
    }

    /// Tells Last.fm what the listener has just started. Not retried: by the time a
    /// retry landed the song would be over.
    pub fn now_playing(self: &Arc<Self>, username: &str, track: LastFmTrack) {
        if !self.is_enabled_for(username)
            || self.active_session(&self.last_fm(), username).is_none()
            || !usable(&track)
        {
            return;
        }
        self.now_playing_in_flight.fetch_add(1, Ordering::SeqCst);
        let service = Arc::clone(self);
        let user = username.trim().to_string();
        tokio::spawn(async move {
            let outcome = AssertUnwindSafe(service.send_now_playing(&user, &track))
                .catch_unwind()
                .await;
            if outcome.is_err() {
                debug!("Last.fm Now Playing failed for {user}");
            }
            service.now_playing_in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// Queues one completed play. The client already decided it counts (Last.fm asks for half
    /// the song or four minutes); the rules left to Octo are that Last.fm takes nothing
    /// shorter than 30 seconds and nothing played more than two weeks ago.
    ///
    /// `chosen_by_user`: false for a play the listener did not pick, such as the next song on an
    /// Octo radio stream. Last.fm is told so. (The C# default was true.)
    pub fn scrobble(
        self: &Arc<Self>,
        username: &str,
        track: LastFmTrack,
        played_at_utc: DateTime<Utc>,
        chosen_by_user: bool,
    ) {
        if !self.is_enabled_for(username) || !usable(&track) {
            return;
        }
        if track.duration_seconds.is_some_and(|d| d > 0 && d < 30) {
            return;
        }
        if played_at_utc < self.now() - Self::OLDEST_PLAY {
            return;
        }
        let user = username.trim();
        {
            let mut gate = self.gate.lock();
            if gate.queues.get(user).is_none() {
                gate.queues.insert(user, Vec::new());
            }
            let queue = gate.queues.get_mut(user).expect("the queue was just made");
            if queue.len() >= Self::MAX_QUEUED_PER_USER {
                warn!("Last.fm queue for {user} is full; dropping the oldest play");
                queue.remove(0);
            }
            queue.push(PendingScrobble {
                id: self.next_play.fetch_add(1, Ordering::SeqCst),
                track,
                played_at_utc,
                chosen_by_user,
                attempts: 0,
            });
            if gate.draining {
                self.wake.notify_one();
                return;
            }
            gate.draining = true;
        }
        tokio::spawn(Arc::clone(self).drain());
    }

    /// Starts linking a Navidrome user to Last.fm: gets a request token and returns the page the
    /// admin opens to approve Octo. This is Last.fm's desktop flow, which needs no address Last.fm
    /// can call back to, so it works for an Octo nobody outside the house can reach.
    pub async fn begin_connect(&self, username: &str) -> Result<String, LastFmScrobbleException> {
        let user = require_username(username)?;
        let settings = self.last_fm();
        if !is_ready_with(&settings) {
            return Err(LastFmScrobbleException::new(
                "Save the Last.fm API key and shared secret first.",
                0,
            ));
        }
        let reply = self
            .call(
                vec![
                    ("method", "auth.getToken".to_string()),
                    ("api_key", settings.api_key.trim().to_string()),
                ],
                settings.api_secret.trim(),
            )
            .await;
        let token = reply
            .body
            .as_ref()
            .and_then(|body| body.get("token"))
            .and_then(Value::as_str)
            .filter(|token| !dotnet::is_blank(token))
            .map(str::to_string);
        let Some(token) = token else {
            return Err(LastFmScrobbleException::new(
                if reply.ok() {
                    "Last.fm sent no token.".to_string()
                } else {
                    describe(&reply)
                },
                reply.error,
            ));
        };
        self.pending_approvals
            .lock()
            .insert(&user, (token.clone(), self.now() + Self::TOKEN_LIFETIME));
        Ok(approval_url(&settings, &token))
    }

    /// Finishes linking once the admin has approved Octo on last.fm, and saves the
    /// session for this Navidrome user.
    pub async fn finish_connect(&self, username: &str) -> Result<LastFmUserSession, LastFmConnectError> {
        let user = require_username(username)?;
        let settings = self.last_fm();
        if !is_ready_with(&settings) {
            return Err(
                LastFmScrobbleException::new("Save the Last.fm API key and shared secret first.", 0).into(),
            );
        }
        let pending = self.pending_approvals.lock().get(&user).cloned();
        let token = match pending {
            Some((token, expires)) if expires >= self.now() => token,
            _ => {
                self.pending_approvals.lock().remove(&user);
                return Err(LastFmScrobbleException::new(
                    "Start with Connect. An approval link lasts an hour.",
                    0,
                )
                .into());
            }
        };

        let reply = self
            .call(
                vec![
                    ("method", "auth.getSession".to_string()),
                    ("api_key", settings.api_key.trim().to_string()),
                    ("token", token),
                ],
                settings.api_secret.trim(),
            )
            .await;
        if !reply.ok() {
            if reply.error == Self::ERROR_TOKEN_NOT_AUTHORIZED {
                return Err(LastFmScrobbleException::new(
                    "Last.fm has not seen the approval yet. Open the link, allow access, then Finish.",
                    reply.error,
                )
                .into());
            }
            let expired = reply.error == Self::ERROR_TOKEN_EXPIRED || reply.error == 4;
            if expired {
                self.pending_approvals.lock().remove(&user);
            }
            return Err(LastFmScrobbleException::new(
                if expired {
                    "That approval link has expired. Connect again.".to_string()
                } else {
                    describe(&reply)
                },
                reply.error,
            )
            .into());
        }

        let session_node = reply.body.as_ref().and_then(|body| body.get("session"));
        let key = session_node
            .and_then(|session| session.get("key"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = session_node
            .and_then(|session| session.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if dotnet::is_blank(&key) {
            return Err(LastFmScrobbleException::new("Last.fm sent no session key.", 0).into());
        }
        let session = LastFmUserSession {
            session_key: key.clone(),
            last_fm_user: name.clone(),
        };

        self.settings_file.update(|root| {
            let sessions = sessions_in(root, true).expect("created when missing");
            for existing in keys_for(sessions, &user) {
                sessions.shift_remove(&existing);
            }
            let mut entry = JsonObject::new();
            entry.insert("SessionKey".to_string(), Node::String(key.clone()));
            entry.insert("LastFmUser".to_string(), Node::String(name.clone()));
            sessions.insert(user.clone(), Node::Object(entry));
            true
        })?;
        self.just_saved.lock().insert(&user, session.clone());
        self.pending_approvals.lock().remove(&user);
        self.revoked_keys.lock().remove(&key);
        self.refused_at.lock().remove(&key);
        self.notices.lock().remove(&user);
        {
            let mut gate = self.gate.lock();
            // Plays that waited on a refused session go with the new one straight away.
            gate.retry_at.remove(&user);
            if gate.draining {
                self.wake.notify_one();
            }
        }
        info!("Last.fm connected for {user} as {name}");
        Ok(session)
    }

    /// Forgets a Connect that is still waiting on its approval.
    pub fn cancel_connect(&self, username: &str) {
        if !dotnet::is_blank(username) {
            self.pending_approvals.lock().remove(username.trim());
        }
    }

    /// Asks Last.fm whether an API key and shared secret work together, before the dashboard saves
    /// them. A blank argument means the saved value. One signed auth.getSession with a token that
    /// was never issued answers all three: error 10 is an unknown key, 13 a signature the secret
    /// does not make for that key, and 4 (the token) means both are right. auth.getToken would
    /// not do: Last.fm hands out a token whatever the signature.
    pub async fn check_credentials(
        &self,
        api_key: Option<&str>,
        api_secret: Option<&str>,
    ) -> LastFmCredentialCheck {
        let settings = self.last_fm();
        let pick = |given: Option<&str>, saved: &str| -> String {
            match given {
                Some(value) if !dotnet::is_blank(value) => value.trim().to_string(),
                _ => saved.trim().to_string(),
            }
        };
        let key = pick(api_key, &settings.api_key);
        let secret = pick(api_secret, &settings.api_secret);
        if key.is_empty() {
            return LastFmCredentialCheck::new(
                "missing",
                if secret.is_empty() { "missing" } else { "unchecked" },
                None,
            );
        }
        // The two look alike, and the key is the one Last.fm shows first, so this is the likely mix-up.
        let secret_state = if secret.is_empty() {
            Some("missing")
        } else if dotnet::eq_ignore_case(&secret, &key) {
            Some("same-as-key")
        } else {
            None
        };

        let reply = self
            .call(
                vec![
                    ("method", "auth.getSession".to_string()),
                    ("api_key", key),
                    ("token", "00000000000000000000000000000000".to_string()),
                ],
                if secret_state.is_none() { &secret } else { "" },
            )
            .await;
        match reply.error {
            10 | 26 => LastFmCredentialCheck::new(
                "invalid",
                secret_state.unwrap_or("unchecked"),
                Some(reply.message),
            ),
            13 => LastFmCredentialCheck::new("ok", secret_state.unwrap_or("invalid"), None),
            4 | 14 | 15 => LastFmCredentialCheck::new("ok", secret_state.unwrap_or("ok"), None),
            0 if reply.ok() => LastFmCredentialCheck::new("ok", secret_state.unwrap_or("ok"), None),
            _ => LastFmCredentialCheck::new(
                "unreachable",
                secret_state.unwrap_or("unchecked"),
                Some(describe(&reply)),
            ),
        }
    }

    /// Stops scrobbling for this user and forgets the session. Returns false when the session
    /// was not in settings.json, which means the environment sets it: it stops now, and comes back
    /// after a restart unless it is removed there too.
    pub fn disconnect(&self, username: &str) -> Result<bool, LastFmScrobbleException> {
        let user = require_username(username)?;
        if let Some(current) = self.saved_session(&self.last_fm(), &user) {
            self.revoked_keys.lock().insert(current.session_key.clone());
            self.refused_at.lock().remove(&current.session_key);
        }
        self.just_saved.lock().remove(&user);
        self.pending_approvals.lock().remove(&user);
        self.notices.lock().remove(&user);
        self.last_sent.lock().remove(&user);
        {
            let mut gate = self.gate.lock();
            gate.queues.remove(&user);
            gate.retry_at.remove(&user);
        }
        let removed = self.remove_saved_session(&user, None);
        info!("Last.fm disconnected for {user}");
        Ok(removed)
    }

    /// Everyone the dashboard should list: the users it already knows of, plus anyone
    /// connected, part way through connecting, or with a notice waiting.
    pub fn users<I, S>(&self, known_users: I) -> Vec<LastFmScrobbleUser>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let settings = self.last_fm();
        let now = self.now();
        let mut names: Vec<String> = known_users.into_iter().map(|s| s.as_ref().to_string()).collect();
        names.extend(settings.user_sessions.keys().cloned());
        names.extend(self.just_saved.lock().keys().map(str::to_string));
        names.extend(self.pending_approvals.lock().keys().map(str::to_string));
        names.extend(self.notices.lock().keys().map(str::to_string));

        let mut distinct: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for name in names.iter().filter(|name| !dotnet::is_blank(name)) {
            let name = name.trim();
            if seen.insert(dotnet::ordinal_ignore_case_key(name)) {
                distinct.push(name.to_string());
            }
        }
        distinct.sort_by(|a, b| dotnet::compare_ordinal_ignore_case(a, b));

        distinct
            .into_iter()
            .map(|name| {
                let session = self.active_session(&settings, &name);
                let pending = self.pending_approvals.lock().get(&name).cloned();
                let waiting = pending.as_ref().is_some_and(|(_, expires)| *expires > now);
                let notice = self.notices.lock().get(&name).cloned();
                let approval_url = match &pending {
                    Some((token, _)) if waiting && is_ready_with(&settings) => {
                        Some(approval_url(&settings, token))
                    }
                    _ => None,
                };
                let last_sent = match &session {
                    Some(_) => self.last_sent.lock().get(&name).cloned(),
                    None => None,
                };
                LastFmScrobbleUser {
                    connected: session.is_some(),
                    last_fm_user: session
                        .as_ref()
                        .map(|s| s.last_fm_user.clone())
                        .filter(|user| !dotnet::is_blank(user)),
                    awaiting_approval: waiting,
                    notice,
                    approval_url,
                    last_sent,
                    user: name,
                }
            })
            .collect()
    }

    async fn send_now_playing(&self, user: &str, track: &LastFmTrack) {
        let settings = self.last_fm();
        let session = self.active_session(&settings, user);
        if self.now() < self.gate.lock().paused_until {
            return;
        }
        let Some(session) = session else { return };
        let mut parameters = vec![
            ("method", "track.updateNowPlaying".to_string()),
            ("api_key", settings.api_key.trim().to_string()),
            ("sk", session.session_key.trim().to_string()),
            ("artist", track.artist.trim().to_string()),
            ("track", track.title.trim().to_string()),
        ];
        if let Some(album) = track.album.as_deref().filter(|a| !dotnet::is_blank(a)) {
            parameters.push(("album", album.trim().to_string()));
        }
        if let Some(duration) = track.duration_seconds.filter(|d| *d > 0) {
            parameters.push(("duration", duration.to_string()));
        }
        let parameters = parameters.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let reply = self.call_owned(parameters, settings.api_secret.trim()).await;
        if reply.ok() {
            self.accepted(user, &session.session_key);
            return;
        }
        if reply.error == Self::ERROR_INVALID_SESSION {
            self.mark_disconnected(user, &session.session_key, &reply.message);
        } else if reply.error == Self::ERROR_RATE_LIMITED {
            let now = self.now();
            self.gate.lock().pause(now, self.tuning.rate_limit_pause);
        } else {
            debug!("Last.fm refused Now Playing for {user}: {}", describe(&reply));
        }
    }

    async fn drain(self: Arc<Self>) {
        loop {
            let (user, batch, wait) = {
                let mut gate = self.gate.lock();
                let next = gate.next_batch(self.now());
                if next.1.is_empty() && next.2 <= TimeDelta::zero() {
                    gate.draining = false;
                    return;
                }
                next
            };
            if batch.is_empty() {
                // A new play wakes the loop early; otherwise it rests until the pause ends,
                // timed on the service's clock.
                let rest = (self.time.sleep)(wait.to_std().unwrap_or_default());
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = rest => {}
                }
                continue;
            }
            let outcome = AssertUnwindSafe(self.send_batch(&user, &batch))
                .catch_unwind()
                .await;
            let failure = match outcome {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some("a panic".to_string()),
            };
            if let Some(failure) = failure {
                // A bug here must not leave plays stuck with nothing draining them.
                warn!(error = %failure, "Last.fm scrobble batch for {user} failed unexpectedly");
                self.gate.lock().forget(&user, &batch);
            }
        }
    }

    /// `Err` where the C# threw out of the method, which the drain caught.
    async fn send_batch(&self, user: &str, batch: &[PendingScrobble]) -> anyhow::Result<()> {
        let settings = self.last_fm();
        let session = self.saved_session(&settings, user);
        let session = match session {
            Some(session) if settings.scrobble_external_plays && is_ready_with(&settings) => session,
            _ => {
                // Disconnected, or switched off, while these waited.
                self.gate.lock().forget(user, batch);
                return Ok(());
            }
        };
        if let Some(resting) = self.resting_until(&session.session_key) {
            // Refused once, by a scrobble or a Now Playing: the plays wait out the grace.
            self.gate.lock().retry_at.insert(user, resting);
            return Ok(());
        }

        let mut parameters: IndexMap<String, String> = IndexMap::new();
        parameters.insert("method".into(), "track.scrobble".into());
        parameters.insert("api_key".into(), settings.api_key.trim().to_string());
        parameters.insert("sk".into(), session.session_key.trim().to_string());
        for (index, play) in batch.iter().enumerate() {
            let track = &play.track;
            parameters.insert(format!("artist[{index}]"), track.artist.trim().to_string());
            parameters.insert(format!("track[{index}]"), track.title.trim().to_string());
            parameters.insert(
                format!("timestamp[{index}]"),
                play.played_at_utc.timestamp().to_string(),
            );
            if let Some(album) = track.album.as_deref().filter(|a| !dotnet::is_blank(a)) {
                parameters.insert(format!("album[{index}]"), album.trim().to_string());
            }
            if let Some(duration) = track.duration_seconds.filter(|d| *d > 0) {
                parameters.insert(format!("duration[{index}]"), duration.to_string());
            }
            if !play.chosen_by_user {
                parameters.insert(format!("chosenByUser[{index}]"), "0".into());
            }
        }

        let reply = self.call_owned(parameters, settings.api_secret.trim()).await;
        if let Some(body) = &reply.body {
            // reply.Body?["scrobbles"]?["@attr"]?["accepted"]: indexing something that is not an
            // object threw, which the drain caught as an unexpected failure.
            let attributes = index(index(Some(body), "scrobbles")?, "@attr")?;
            let accepted = number(index(attributes, "accepted")?);
            let ignored = number(index(attributes, "ignored")?);
            info!(
                "Last.fm took {} of {} plays for {user} ({} ignored)",
                accepted.unwrap_or(batch.len() as i32),
                batch.len(),
                ignored.unwrap_or(0)
            );
            {
                let mut gate = self.gate.lock();
                gate.rate_limit_strikes = 0;
                gate.retry_at.remove(user);
                gate.forget(user, batch);
            }
            self.accepted(user, &session.session_key);
            if accepted.unwrap_or(batch.len() as i32) > 0 {
                // MaxBy: the first of the latest.
                let mut latest = &batch[0];
                for play in &batch[1..] {
                    if play.played_at_utc > latest.played_at_utc {
                        latest = play;
                    }
                }
                self.last_sent.lock().insert(
                    user,
                    LastFmSentPlay {
                        artist: latest.track.artist.trim().to_string(),
                        title: latest.track.title.trim().to_string(),
                        played_at_utc: latest.played_at_utc,
                    },
                );
            }
            return Ok(());
        }

        match reply.error {
            Self::ERROR_INVALID_SESSION => {
                self.mark_disconnected(user, &session.session_key, &reply.message);
                let revoked = self.revoked_keys.lock().contains(&session.session_key);
                let resting = self.resting_until(&session.session_key);
                let now = self.now();
                let mut gate = self.gate.lock();
                if revoked {
                    // Refused again after the grace: the listener did revoke Octo.
                    gate.queues.remove(user);
                    gate.retry_at.remove(user);
                } else {
                    // The first refusal. The plays stay queued and nothing is sent for this
                    // listener until the grace is over.
                    gate.retry_at
                        .insert(user, resting.unwrap_or(now + self.tuning.refusal_grace));
                }
            }
            Self::ERROR_RATE_LIMITED => {
                let now = self.now();
                self.gate.lock().pause(now, self.tuning.rate_limit_pause);
                warn!("Last.fm asked Octo to slow down; scrobbles wait before the next try");
            }
            _ if reply.retryable => {
                // Unreachable, a server error, or one of Last.fm's own "try again later"
                // answers (11, 16). Error 8 is not one: it comes back the same each time.
                let now = self.now();
                let mut gate = self.gate.lock();
                let mut attempts = 0;
                let mut spent = Vec::new();
                for play in batch {
                    let tried = play.attempts + 1;
                    if let Some(queued) = gate
                        .queues
                        .get_mut(user)
                        .and_then(|queue| queue.iter_mut().find(|queued| queued.id == play.id))
                    {
                        queued.attempts = tried;
                    }
                    attempts = attempts.max(tried);
                    if tried >= Self::MAX_ATTEMPTS {
                        spent.push(play.clone());
                    }
                }
                if !spent.is_empty() {
                    warn!(
                        "Giving up on {} Last.fm plays for {user} after {} tries: {}",
                        spent.len(),
                        Self::MAX_ATTEMPTS,
                        describe(&reply)
                    );
                    gate.forget(user, &spent);
                }
                let factor = 1i32 << (attempts - 1).clamp(0, 10);
                let delay = (self.tuning.retry_delay * factor).min(Self::LONGEST_RETRY_WAIT);
                gate.retry_at.insert(user, now + delay);
            }
            _ => {
                // Anything else will be refused the same way next time, so retrying only delays the rest.
                warn!(
                    "Last.fm refused {} plays for {user}: {}",
                    batch.len(),
                    describe(&reply)
                );
                self.gate.lock().forget(user, batch);
            }
        }
        Ok(())
    }

    /// Last.fm no longer accepts this session, which is what happens when the listener removes
    /// Octo from their Last.fm applications. The first refusal rests the session in memory and
    /// the dashboard says why; the saved session is kept, and so are the listener's plays, which
    /// keep queueing, because Last.fm has been known to say this once and mean nothing by it.
    /// After the refusal grace the session is tried again, and a second refusal then removes it
    /// from settings.json for good.
    fn mark_disconnected(&self, user: &str, session_key: &str, detail: &str) {
        let now = self.now();
        let why = if dotnet::is_blank(detail) {
            String::new()
        } else {
            format!(" ({})", detail.trim())
        };
        let stamp = now.format("%Y-%m-%d %H:%M");
        let first = self.refused_at.lock().get(session_key).copied();
        if let Some(first) = first
            && now - first >= self.tuning.refusal_grace
        {
            self.revoked_keys.lock().insert(session_key.to_string());
            self.refused_at.lock().remove(session_key);
            self.notices.lock().insert(
                user,
                format!("Last.fm stopped accepting this connection on {stamp} UTC{why}")
                    + ". Connect again to resume scrobbling.",
            );
            warn!("Last.fm refused the session for {user} again; it is removed until they connect again");
            self.remove_saved_session(user, Some(session_key));
            return;
        }
        self.refused_at
            .lock()
            .entry(session_key.to_string())
            .or_insert(now);
        self.notices.lock().insert(
            user,
            format!("Last.fm refused this connection on {stamp} UTC{why}")
                + ". Scrobbling for this listener is paused and their plays wait. Octo tries once more after an hour and"
                + " removes the connection if Last.fm still refuses it. Connect again to resume now.",
        );
        warn!("Last.fm refused the session for {user}; scrobbling for them is paused");
    }

    /// A call with this session went through, so an earlier refusal was not meant.
    fn accepted(&self, user: &str, session_key: &str) {
        if self.refused_at.lock().remove(session_key).is_some() {
            self.notices.lock().remove(user);
        }
    }

    /// Removes the user's saved session. With `only_key`, only when it is still that key, so a
    /// refusal of an old session cannot undo a reconnect that happened meanwhile.
    fn remove_saved_session(&self, user: &str, only_key: Option<&str>) -> bool {
        let result = self.settings_file.update(|root| {
            let Some(sessions) = sessions_in(root, false) else {
                return false;
            };
            let matches: Vec<String> = keys_for(sessions, user)
                .into_iter()
                .filter(|key| {
                    only_key.is_none_or(|only| {
                        sessions
                            .get(key)
                            .and_then(|entry| entry.get("SessionKey"))
                            .and_then(Node::as_str)
                            == Some(only)
                    })
                })
                .collect();
            for key in &matches {
                sessions.shift_remove(key);
            }
            !matches.is_empty()
        });
        match result {
            Ok(removed) => removed,
            Err(e) => {
                // The key is already revoked in memory, so this only matters after a restart.
                warn!(error = %e, "Could not remove the Last.fm session for {user} from settings");
                false
            }
        }
    }

    /// The listener's session unless Last.fm has revoked it. It may be resting.
    fn saved_session(&self, settings: &LastFmSettings, username: &str) -> Option<LastFmUserSession> {
        let mut session = settings.session_for(username).cloned();
        {
            let mut just_saved = self.just_saved.lock();
            if let Some(fresh) = just_saved.get(username.trim()).cloned() {
                // The reload has caught up once the settings carry the same key.
                if session.as_ref().map(|s| &s.session_key) == Some(&fresh.session_key) {
                    just_saved.remove(username.trim());
                } else {
                    session = Some(fresh);
                }
            }
        }
        session.filter(|session| !self.revoked_keys.lock().contains(&session.session_key))
    }

    /// The listener's session when it may be used right now: not revoked, and not
    /// resting after a refusal.
    fn active_session(&self, settings: &LastFmSettings, username: &str) -> Option<LastFmUserSession> {
        self.saved_session(settings, username)
            .filter(|session| self.resting_until(&session.session_key).is_none())
    }

    /// When a session Last.fm refused once may be tried again, or None when it is not resting.
    fn resting_until(&self, session_key: &str) -> Option<DateTime<Utc>> {
        let refused = self.refused_at.lock().get(session_key).copied()?;
        (self.now() - refused < self.tuning.refusal_grace).then(|| refused + self.tuning.refusal_grace)
    }

    async fn call(&self, parameters: Vec<(&str, String)>, secret: &str) -> Reply {
        self.call_owned(
            parameters.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            secret,
        )
        .await
    }

    async fn call_owned(&self, mut parameters: IndexMap<String, String>, secret: &str) -> Reply {
        let signature = sign(parameters.iter().map(|(k, v)| (k.as_str(), v.as_str())), secret);
        parameters.insert("api_sig".into(), signature);
        parameters.insert("format".into(), "json".into());
        // FormUrlEncodedContent: each name and value escaped, joined in the dictionary's order.
        let body = parameters
            .iter()
            .map(|(k, v)| format!("{}={}", dotnet::form_url_encode(k), dotnet::form_url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let sent = self
            .http
            .post(&self.api_url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await;
        let answer = match sent {
            Ok(response) => HttpAnswer::read(response).await,
            Err(e) => Err(e),
        };
        let answer = match answer {
            Ok(answer) => answer,
            Err(e) => {
                return Reply {
                    body: None,
                    error: 0,
                    message: failure_message(&e),
                    retryable: true,
                };
            }
        };
        // Last.fm puts a refusal in the body under a 4xx status, so the body decides, not the status.
        let document = match serde_json::from_str::<Value>(&answer.text()) {
            Ok(document @ Value::Object(_)) => Some(document),
            // Not JSON: judged by the status below.
            _ => None,
        };
        if let Some(document) = &document
            && let Some(code) = document
                .get("error")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
        {
            return Reply {
                body: None,
                error: code,
                message: document
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                retryable: code == 11 || code == 16,
            };
        }
        if answer.is_success()
            && let Some(document) = document
        {
            return Reply {
                body: Some(document),
                error: 0,
                message: String::new(),
                retryable: false,
            };
        }
        Reply {
            body: None,
            error: 0,
            message: format!("HTTP {}", answer.status.as_u16()),
            retryable: answer.status.as_u16() >= 500,
        }
    }
}

/// The `Message` of the `HttpRequestException` or `TaskCanceledException` a failed call threw.
fn failure_message(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        timeout_message(LastFmScrobbleService::CLIENT_TIMEOUT)
    } else {
        connect_failure_message(error)
    }
}

fn describe(reply: &Reply) -> String {
    if reply.error == 0 {
        format!("Last.fm could not be reached: {}", reply.message)
    } else {
        format!("Last.fm error {}: {}", reply.error, reply.message)
    }
}

fn is_ready_with(settings: &LastFmSettings) -> bool {
    !dotnet::is_blank(&settings.api_key) && !dotnet::is_blank(&settings.api_secret)
}

fn usable(track: &LastFmTrack) -> bool {
    !dotnet::is_blank(&track.artist) && !dotnet::is_blank(&track.title)
}

fn approval_url(settings: &LastFmSettings, token: &str) -> String {
    format!(
        "{}?api_key={}&token={}",
        LastFmScrobbleService::AUTH_URL,
        dotnet::escape_data_string(settings.api_key.trim()),
        dotnet::escape_data_string(token)
    )
}

/// A Navidrome username fit to be a settings key. A colon would split it into two
/// levels, because that is how configuration writes a path.
fn require_username(username: &str) -> Result<String, LastFmScrobbleException> {
    let user = username.trim();
    let length = dotnet::utf16_len(user);
    if length == 0 || length > 100 || user.contains(':') || user.chars().any(char::is_control) {
        return Err(LastFmScrobbleException::new(
            "A Navidrome username is required.",
            0,
        ));
    }
    Ok(user.to_string())
}

/// The `LastFm.UserSessions` object in settings.json, found ignoring case, and made when
/// `create` (a section or dictionary that is not an object is replaced).
fn sessions_in(root: &mut JsonObject, create: bool) -> Option<&mut JsonObject> {
    let section_key = root
        .keys()
        .find(|key| dotnet::eq_ignore_case(key, "LastFm"))
        .cloned();
    let section_key = match section_key {
        Some(key) if root.get(&key).is_some_and(Node::is_object) => key,
        found => {
            if !create {
                return None;
            }
            let key = found.unwrap_or_else(|| "LastFm".to_string());
            root.insert(key.clone(), Node::object());
            key
        }
    };
    let section = root.get_mut(&section_key)?.as_object_mut()?;
    let sessions_key = section
        .keys()
        .find(|key| dotnet::eq_ignore_case(key, "UserSessions"))
        .cloned();
    let sessions_key = match sessions_key {
        Some(key) if section.get(&key).is_some_and(Node::is_object) => key,
        found => {
            if !create {
                return None;
            }
            let key = found.unwrap_or_else(|| "UserSessions".to_string());
            section.insert(key.clone(), Node::object());
            key
        }
    };
    section.get_mut(&sessions_key)?.as_object_mut()
}

fn keys_for(sessions: &JsonObject, user: &str) -> Vec<String> {
    sessions
        .keys()
        .filter(|key| dotnet::eq_ignore_case(key.trim(), user))
        .cloned()
        .collect()
}

/// `node?["name"]` on a `JsonNode`: null stays null, an object gives its property, and anything
/// else throws.
fn index<'a>(node: Option<&'a Value>, name: &str) -> anyhow::Result<Option<&'a Value>> {
    match node {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(map.get(name).filter(|value| !value.is_null())),
        Some(_) => anyhow::bail!("The node must be of type 'JsonObject'."),
    }
}

/// A JSON number that fits an int, or a string that parses as one.
fn number(node: Option<&Value>) -> Option<i32> {
    match node? {
        Value::Number(number) => number.as_i64().and_then(|n| i32::try_from(n).ok()),
        Value::String(text) => parse_long(text).and_then(|n| i32::try_from(n).ok()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "last_fm_scrobble_service_tests.rs"]
mod tests;
