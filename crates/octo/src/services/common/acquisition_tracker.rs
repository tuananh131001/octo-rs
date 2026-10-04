//! Port of `Services/Common/AcquisitionTracker.cs`. The pure helpers (`FractionOf`, `UserSafe`)
//! are `octo_core::common::acquisition_tracker`.
//!
//! The C# guarded every method with a catch-all, because "a bookkeeping mistake must never cost
//! anyone a song". Nothing here can fail short of a panic, and a listener's panic is caught and
//! logged, as a throwing listener was.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::common::acquisition_tracker::{fraction_of, user_safe};
use octo_core::common::dotnet::{self, compare_ordinal_ignore_case, eq_ignore_case, to_lower_invariant};
use parking_lot::Mutex;
use tracing::{debug, info};

use crate::services::framework::DotnetDictionary;
use crate::services::library::NavidromeSongPathResolver;
use crate::services::local::ILocalLibraryService;
use crate::services::subsonic::NavidromeIdentityService;

/// Where one acquisition has got to. Sent to clients in lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcquisitionState {
    /// Accepted, waiting for the worker.
    Queued,
    /// Looking for a source or a peer.
    Searching,
    /// Bytes are flowing.
    Downloading,
    /// The duration check and the fingerprint.
    Verifying,
    /// Placed, tagged and registered; waiting for Navidrome to show it.
    Importing,
    Done,
    Failed,
}

impl AcquisitionState {
    /// The member's name in lower case, as clients are sent it.
    pub fn wire_name(self) -> &'static str {
        match self {
            AcquisitionState::Queued => "queued",
            AcquisitionState::Searching => "searching",
            AcquisitionState::Downloading => "downloading",
            AcquisitionState::Verifying => "verifying",
            AcquisitionState::Importing => "importing",
            AcquisitionState::Done => "done",
            AcquisitionState::Failed => "failed",
        }
    }
}

/// One acquisition as a reader sees it. A copy, so it never changes under them.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquisitionSnapshot {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub requested_by: Vec<String>,
    pub source: Option<String>,
    pub state: AcquisitionState,
    pub progress: Option<f64>,
    pub bytes_done: Option<i64>,
    pub bytes_total: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error: Option<String>,
    pub library_id: Option<String>,
    pub ahead: Option<i32>,
    pub note: Option<String>,
}

/// How a row ended. `library_id` is set only for a song Navidrome showed; `album_keys` are
/// the hearted albums the song was fetched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionEnd {
    pub key: String,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub done: bool,
    pub library_id: Option<String>,
    pub album_keys: Vec<String>,
}

/// Finds the Navidrome id of a placed file from its artist, title and path. `Err` where the
/// C# lookup threw.
pub type LibraryLookup =
    Arc<dyn Fn(String, String, String) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;

/// Asks Navidrome to scan.
pub type Rescan = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Told when a row ends (the C# `Ended` event).
pub type EndedListener = Arc<dyn Fn(&AcquisitionEnd) + Send + Sync>;

/// What the tracker looks in Navidrome with: the services the C# resolved lazily from its
/// `IServiceProvider` when a song was imported. None of them depends on the tracker, so they
/// are simply handed over at construction.
#[derive(Clone)]
pub struct TrackerServices {
    pub identity: NavidromeIdentityService,
    pub resolver: Arc<NavidromeSongPathResolver>,
    pub library: Arc<dyn ILocalLibraryService>,
}

/// How an imported song is looked for in Navidrome, and the test seams (`Rescan`,
/// `LibraryLookup`), which the C# exposed as internal settable properties.
#[derive(Clone)]
pub struct WatchSettings {
    /// Every `visibility_poll` for `visibility_attempts` tries, then every
    /// `slow_visibility_poll` for `slow_visibility_attempts` more. About ten minutes in all,
    /// because a song the app is waiting on should arrive with its id, not after the next full
    /// sync.
    pub visibility_poll: Duration,
    pub visibility_attempts: i32,
    pub slow_visibility_poll: Duration,
    pub slow_visibility_attempts: i32,

    /// After this many tries without the song, Navidrome is asked to scan once, past the
    /// debounce. A scan takes seconds, so a song still missing after a minute usually means the
    /// scan that should have found it was swallowed by the debounce behind another one.
    pub rescan_after_attempts: i32,

    /// Asks Navidrome to scan. Tests set it; otherwise the library service does.
    pub rescan: Option<Rescan>,

    /// Finds the Navidrome id of a placed file. `None` (with no services) means nothing can
    /// look, and an imported song is then done at once. Tests set it; otherwise the song path
    /// resolver answers.
    pub library_lookup: Option<LibraryLookup>,
}

impl Default for WatchSettings {
    fn default() -> Self {
        WatchSettings {
            visibility_poll: Duration::from_secs(5),
            visibility_attempts: 36,
            slow_visibility_poll: Duration::from_secs(15),
            slow_visibility_attempts: 28,
            rescan_after_attempts: 12,
            rescan: None,
            library_lookup: None,
        }
    }
}

struct Entry {
    provider: String,
    external_id: String,
    client_id: Option<String>,
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
    /// Case-insensitive, first spelling kept.
    owners: Vec<String>,
    source: Option<String>,
    state: AcquisitionState,
    progress: Option<f64>,
    bytes_done: Option<i64>,
    bytes_total: Option<i64>,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    error: Option<String>,
    library_id: Option<String>,

    /// A short line for the listener, such as which source is being tried now.
    note: Option<String>,

    /// Order of arrival. Tracks an album walk lists together share a start time,
    /// so this, not the time, is what says which of them is ahead.
    seq: i64,

    /// Bumped on every restart, so a watcher left over from an earlier run of the
    /// same song can never finish the new one.
    run: i32,
}

impl Entry {
    fn finished(&self) -> bool {
        matches!(self.state, AcquisitionState::Done | AcquisitionState::Failed)
    }
}

/// Who hearted an album, so its tracks are theirs when the walk lists them.
struct AlbumClaim {
    owners: Vec<String>,
    track_keys: Vec<String>,
    updated_at: DateTime<Utc>,
}

#[derive(Default)]
struct State {
    seq: i64,
    entries: DotnetDictionary<Entry>,
    albums: DotnetDictionary<AlbumClaim>,
}

/// Live progress of every hearted download, from the moment the star is accepted until a while
/// after it ends, so a client can draw a ring on the button it was tapped on.
///
/// Observation only. Nothing in the download path reads it back, and no method here fails: a
/// bookkeeping mistake must never cost anyone a song. Held in memory and gone on a restart,
/// which is fine, because the fetched-songs log still says what finished.
///
/// Keyed by provider and external id, the two things every stage of the pipeline already has.
/// The id the client starred rides along so the client can find its own row again.
pub struct AcquisitionTracker {
    state: Mutex<State>,
    services: Option<TrackerServices>,
    clock: Clock,
    watch: Mutex<WatchSettings>,
    listeners: Mutex<Vec<(u64, EndedListener)>>,
    next_listener: AtomicU64,
}

/// The key every stage of the pipeline files a song under.
pub fn key_of(provider: &str, external_id: &str) -> String {
    format!("{}:{external_id}", to_lower_invariant(provider.trim()))
}

fn add_owner(owners: &mut Vec<String>, username: Option<&str>) {
    if let Some(username) = username.filter(|u| !dotnet::is_blank(u)) {
        let username = username.trim();
        if !owners.iter().any(|o| eq_ignore_case(o, username)) {
            owners.push(username.to_string());
        }
    }
}

fn union_owners(owners: &mut Vec<String>, more: &[String]) {
    for owner in more {
        add_owner(owners, Some(owner));
    }
}

fn name(entry: &mut Entry, artist: Option<&str>, title: Option<&str>, album: Option<&str>) {
    if let Some(artist) = artist.filter(|a| !dotnet::is_blank(a)) {
        entry.artist = Some(artist.to_string());
    }
    if let Some(title) = title.filter(|t| !dotnet::is_blank(t)) {
        entry.title = Some(title.to_string());
    }
    if let Some(album) = album.filter(|a| !dotnet::is_blank(a)) {
        entry.album = Some(album.to_string());
    }
}

fn blank(value: &str) -> bool {
    dotnet::is_blank(value)
}

impl AcquisitionTracker {
    /// How long a finished or failed entry stays visible.
    pub const FINISHED_RETENTION: TimeDelta = TimeDelta::minutes(30);

    /// How long an entry that is still running may go without any news before it is dropped.
    /// Generous on purpose: a star can wait behind a whole album for hours. This only exists so
    /// a run that ended somewhere nothing reported it cannot sit in the list forever.
    pub const STALLED_RETENTION: TimeDelta = TimeDelta::hours(24);

    pub const CAPACITY: usize = 500;

    /// `services` are what an imported song is looked for in Navidrome with; `None` (as the
    /// tests built it, `services: null`) means only the [`WatchSettings`] seams can look.
    pub fn new(services: Option<TrackerServices>, clock: Clock) -> Self {
        AcquisitionTracker {
            state: Mutex::new(State::default()),
            services,
            clock,
            watch: Mutex::new(WatchSettings::default()),
            listeners: Mutex::new(Vec::new()),
            next_listener: AtomicU64::new(1),
        }
    }

    /// Changes how an imported song is watched for (the C# internal properties).
    pub fn configure_watch(&self, change: impl FnOnce(&mut WatchSettings)) {
        change(&mut self.watch.lock());
    }

    /// Subscribes to rows ending (`Ended += listener`). Told outside the lock. A listener that
    /// panics is logged and skipped, so it can never cost anyone a song.
    pub fn subscribe_ended(&self, listener: EndedListener) -> u64 {
        let id = self.next_listener.fetch_add(1, Ordering::SeqCst);
        self.listeners.lock().push((id, listener));
        id
    }

    /// `Ended -= listener`.
    pub fn unsubscribe_ended(&self, id: u64) {
        self.listeners.lock().retain(|(listener, _)| *listener != id);
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    // ---------------------------------------------------------------------------------------
    // Starting
    // ---------------------------------------------------------------------------------------

    /// A star on a song was accepted. Starts an entry, or restarts one that already finished,
    /// or adds this user to one that is still running (a second heart joins the same download).
    #[allow(clippy::too_many_arguments)] // the C# signature, member for member
    pub fn begin(
        &self,
        provider: &str,
        external_id: &str,
        client_id: Option<&str>,
        requested_by: Option<&str>,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) {
        if blank(provider) || blank(external_id) {
            return;
        }
        let now = self.now();
        let mut state = self.state.lock();
        let entry = open(&mut state, provider, external_id, true, now);
        if let Some(client_id) = client_id.filter(|c| !blank(c)) {
            entry.client_id = Some(client_id.to_string());
        }
        add_owner(&mut entry.owners, requested_by);
        name(entry, artist, title, album);
        prune(&mut state, now);
    }

    /// A star on an album was accepted. Its tracks are listed as the walk reaches them.
    pub fn begin_album(&self, provider: &str, album_id: &str, requested_by: Option<&str>) {
        if blank(provider) || blank(album_id) {
            return;
        }
        let now = self.now();
        let mut state = self.state.lock();
        let key = key_of(provider, album_id);
        if !state.albums.contains_key(&key) {
            state.albums.set(
                key.clone(),
                AlbumClaim {
                    owners: Vec::new(),
                    track_keys: Vec::new(),
                    updated_at: now,
                },
            );
        }
        let claim = state.albums.get_mut(&key).expect("just made");
        add_owner(&mut claim.owners, requested_by);
        claim.updated_at = now;
        prune(&mut state, now);
    }

    /// An album walk is about to fetch these tracks (external id, artist, title, album). They
    /// belong to whoever hearted the album, or whoever hearted the song that started the walk.
    /// A walk nobody hearted (a play in Album mode) lists nothing. A track already running or
    /// already done is left as it is; one that failed before is queued again, because this is
    /// another go at it.
    pub fn announce(
        &self,
        provider: &str,
        album_id: Option<&str>,
        parent_external_id: Option<&str>,
        tracks: &[(String, Option<String>, Option<String>, Option<String>)],
    ) {
        if blank(provider) {
            return;
        }
        let now = self.now();
        let mut state = self.state.lock();
        let mut owners = Vec::new();
        let claim_key = album_id
            .filter(|a| !blank(a))
            .map(|a| key_of(provider, a))
            .filter(|k| state.albums.contains_key(k));
        if let Some(claim) = claim_key.as_deref().and_then(|k| state.albums.get(k)) {
            union_owners(&mut owners, &claim.owners);
        }
        if let Some(parent) = parent_external_id
            .filter(|p| !blank(p))
            .and_then(|p| state.entries.get(&key_of(provider, p)))
        {
            union_owners(&mut owners, &parent.owners);
        }
        if owners.is_empty() {
            return;
        }

        for (external_id, artist, title, album) in tracks {
            if blank(external_id) {
                continue;
            }
            let key = key_of(provider, external_id);
            if let Some(claim) = claim_key.as_deref().and_then(|k| state.albums.get_mut(k))
                && !claim.track_keys.contains(&key)
            {
                claim.track_keys.push(key.clone());
            }
            let restart = state
                .entries
                .get(&key)
                .is_none_or(|existing| existing.state == AcquisitionState::Failed);
            let entry = if restart {
                open(&mut state, provider, external_id, true, now)
            } else {
                state.entries.get_mut(&key).expect("checked above")
            };
            union_owners(&mut entry.owners, &owners);
            name(entry, artist.as_deref(), title.as_deref(), album.as_deref());
        }
        if let Some(claim) = claim_key.as_deref().and_then(|k| state.albums.get_mut(k)) {
            claim.updated_at = now;
        }
        prune(&mut state, now);
    }

    // ---------------------------------------------------------------------------------------
    // Moving along. Every one of these touches only an entry that is still running, so a stage
    // reported by a play of a song that was hearted an hour ago cannot reopen a finished row.
    // ---------------------------------------------------------------------------------------

    /// Move to a stage, and name the source when it is known. A note replaces the last one; a
    /// stage without one keeps it, so "trying YouTube" stays up while YouTube searches.
    pub fn stage(
        &self,
        provider: &str,
        external_id: &str,
        state: AcquisitionState,
        source: Option<&str>,
        note: Option<&str>,
    ) {
        match state {
            AcquisitionState::Done => return self.complete(provider, external_id, None),
            AcquisitionState::Failed => return self.fail(provider, external_id, None),
            _ => {}
        }
        self.update(provider, external_id, true, |entry| {
            // Back to looking means the last transfer is gone, whatever it had reached.
            if matches!(state, AcquisitionState::Queued | AcquisitionState::Searching) {
                entry.bytes_done = None;
                entry.bytes_total = None;
            }
            entry.state = state;
            entry.progress = None;
            if let Some(source) = source.filter(|s| !blank(s)) {
                entry.source = Some(source.to_string());
            }
            if let Some(note) = note.filter(|n| !blank(n)) {
                entry.note = Some(note.trim().to_string());
            }
        });
    }

    /// Fill in the names once the pipeline has them. Empty values never overwrite.
    pub fn describe(
        &self,
        provider: &str,
        external_id: &str,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) {
        self.update(provider, external_id, false, |entry| {
            name(entry, artist, title, album)
        });
    }

    /// A transfer started or moved on. Any figure may be missing; progress comes from the bytes
    /// when both are known, otherwise from the percentage. Nothing moved yet reads as unknown
    /// rather than zero: slskd reports a transfer in progress before its first byte, and a
    /// ring drawn at 0% looks like a download that died.
    pub fn transfer(
        &self,
        provider: &str,
        external_id: &str,
        bytes_done: Option<i64>,
        bytes_total: Option<i64>,
        percent_complete: Option<f64>,
        source: Option<&str>,
    ) {
        self.update(provider, external_id, true, |entry| {
            entry.state = AcquisitionState::Downloading;
            if let Some(source) = source.filter(|s| !blank(s)) {
                entry.source = Some(source.to_string());
            }
            entry.bytes_done = bytes_done.filter(|b| *b >= 0);
            entry.bytes_total = bytes_total.filter(|b| *b > 0);
            entry.progress = fraction_of(bytes_done, bytes_total, percent_complete).filter(|f| *f > 0.0);
        });
    }

    /// The file is in the library folder and registered. The entry reads importing until
    /// Navidrome can see the song, then done with its id; if nothing can look, or Navidrome has
    /// not shown it within a few minutes, done without one. The file is there either way.
    pub fn imported(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        artist: Option<&str>,
        title: Option<&str>,
        path: Option<&str>,
    ) {
        let key = key_of(provider, external_id);
        let (run, artist, title) = {
            let now = self.now();
            let mut state = self.state.lock();
            let Some(entry) = state.entries.get_mut(&key).filter(|e| !e.finished()) else {
                return;
            };
            entry.state = AcquisitionState::Importing;
            entry.progress = None;
            entry.updated_at = now;
            // A song found already in the library arrives here before anything named it.
            let artist = artist
                .filter(|a| !blank(a))
                .map(str::to_string)
                .or_else(|| entry.artist.clone());
            let title = title
                .filter(|t| !blank(t))
                .map(str::to_string)
                .or_else(|| entry.title.clone());
            (entry.run, artist, title)
        };

        let lookup = self.resolve_lookup();
        let runtime = tokio::runtime::Handle::try_current().ok();
        match (
            lookup,
            path.filter(|p| !blank(p)),
            artist.filter(|a| !blank(a)),
            title.filter(|t| !blank(t)),
            runtime,
        ) {
            (Some(lookup), Some(path), Some(artist), Some(title), Some(runtime)) => {
                let tracker = Arc::clone(self);
                let path = path.to_string();
                runtime.spawn(async move {
                    tracker
                        .watch_until_visible(key, run, lookup, artist, title, path)
                        .await
                });
            }
            _ => self.finish(&key, run, None),
        }
    }

    /// In the library.
    pub fn complete(&self, provider: &str, external_id: &str, library_id: Option<&str>) {
        let key = key_of(provider, external_id);
        let run = {
            let state = self.state.lock();
            match state.entries.get(&key) {
                Some(entry) if !entry.finished() => entry.run,
                _ => return,
            }
        };
        self.finish(&key, run, library_id);
    }

    /// The last source gave up. Call this only at the end of the chain: a source that fails
    /// while another is still to try is not a failure the listener should see.
    pub fn fail(&self, provider: &str, external_id: &str, error: Option<&str>) {
        if blank(provider) || blank(external_id) {
            return;
        }
        let key = key_of(provider, external_id);
        let ended = {
            let now = self.now();
            let mut state = self.state.lock();
            let Some(entry) = state.entries.get_mut(&key).filter(|e| !e.finished()) else {
                return;
            };
            entry.state = AcquisitionState::Failed;
            entry.progress = None;
            entry.error = Some(user_safe(error).unwrap_or_else(|| "The download failed.".to_string()));
            entry.updated_at = now;
            end_of(&state, &key)
        };
        self.raise(ended);
    }

    /// Fail every track of a hearted album that is still running.
    pub fn fail_album(&self, provider: &str, album_id: &str, error: Option<&str>) {
        let keys = {
            let state = self.state.lock();
            match state.albums.get(&key_of(provider, album_id)) {
                Some(claim) => claim.track_keys.clone(),
                None => return,
            }
        };
        for key in keys {
            if let Some((provider, external_id)) = key.split_once(':') {
                self.fail(provider, external_id, error);
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Reading
    // ---------------------------------------------------------------------------------------

    /// Everything, newest first. For the dashboard.
    pub fn all(&self) -> Vec<AcquisitionSnapshot> {
        self.read(None)
    }

    /// Only the entries this user asked for, newest first.
    pub fn for_user(&self, username: &str) -> Vec<AcquisitionSnapshot> {
        if blank(username) {
            return Vec::new();
        }
        self.read(Some(username.trim()))
    }

    fn read(&self, username: Option<&str>) -> Vec<AcquisitionSnapshot> {
        let now = self.now();
        let mut state = self.state.lock();
        prune(&mut state, now);
        let running: Vec<(i64, &Entry)> = state
            .entries
            .values()
            .filter(|entry| !entry.finished())
            .map(|entry| (entry.seq, entry))
            .collect();
        let mut rows: Vec<&Entry> = state
            .entries
            .values()
            .filter(|entry| username.is_none_or(|u| entry.owners.iter().any(|o| eq_ignore_case(o, u))))
            .collect();
        // OrderByDescending(StartedAt), stable.
        rows.sort_by_key(|row| std::cmp::Reverse(row.started_at));
        rows.into_iter()
            .map(|entry| snapshot(entry, ahead_of(entry, &running)))
            .collect()
    }

    // ---------------------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------------------

    fn update(&self, provider: &str, external_id: &str, touch: bool, change: impl FnOnce(&mut Entry)) {
        if blank(provider) || blank(external_id) {
            return;
        }
        let now = self.now();
        let mut state = self.state.lock();
        let Some(entry) = state
            .entries
            .get_mut(&key_of(provider, external_id))
            .filter(|e| !e.finished())
        else {
            return;
        };
        change(entry);
        if touch {
            entry.updated_at = now;
        }
    }

    fn finish(&self, key: &str, run: i32, library_id: Option<&str>) {
        let ended = {
            let now = self.now();
            let mut state = self.state.lock();
            let Some(entry) = state
                .entries
                .get_mut(key)
                .filter(|e| !e.finished() && e.run == run)
            else {
                return;
            };
            entry.state = AcquisitionState::Done;
            entry.progress = None;
            entry.error = None;
            entry.note = None;
            if let Some(library_id) = library_id.filter(|l| !blank(l)) {
                entry.library_id = Some(library_id.to_string());
            }
            entry.updated_at = now;
            end_of(&state, key)
        };
        self.raise(ended);
    }

    fn raise(&self, ended: Option<AcquisitionEnd>) {
        let Some(ended) = ended else { return };
        let listeners: Vec<EndedListener> = self.listeners.lock().iter().map(|(_, l)| l.clone()).collect();
        for listener in listeners {
            if catch_unwind(AssertUnwindSafe(|| listener(&ended))).is_err() {
                debug!("Acquisition end listener failed");
            }
        }
    }

    async fn watch_until_visible(
        self: Arc<Self>,
        key: String,
        run: i32,
        lookup: LibraryLookup,
        artist: String,
        title: String,
        path: String,
    ) {
        let watch = self.watch.lock().clone();
        let fast = watch.visibility_attempts.max(1);
        let total = fast + watch.slow_visibility_attempts.max(0);
        for attempt in 0..total {
            if attempt > 0 {
                tokio::time::sleep(if attempt < fast {
                    watch.visibility_poll
                } else {
                    watch.slow_visibility_poll
                })
                .await;
            }
            if attempt > 0 && attempt == watch.rescan_after_attempts {
                self.rescan_once(&path, watch.rescan.clone()).await;
            }
            {
                // Restarted or finished by something else while this waited: not ours now.
                let state = self.state.lock();
                match state.entries.get(&key) {
                    Some(entry) if entry.run == run && !entry.finished() => {}
                    _ => return,
                }
            }
            let id = match AssertUnwindSafe(lookup(artist.clone(), title.clone(), path.clone()))
                .catch_unwind()
                .await
            {
                Ok(Ok(id)) => id,
                Ok(Err(e)) => {
                    debug!("Library lookup for {path} failed: {e}");
                    None
                }
                Err(_) => {
                    debug!("Library lookup for {path} failed");
                    None
                }
            };
            if let Some(id) = id.filter(|i| !blank(i)) {
                self.finish(&key, run, Some(&id));
                return;
            }
        }
        // Registered and in the folder; Navidrome is just slow to say so. Done is still the
        // truth, and the app finds the song on its own from here.
        self.finish(&key, run, None);
    }

    async fn rescan_once(&self, path: &str, rescan: Option<Rescan>) {
        let Some(rescan) = rescan.or_else(|| self.resolve_rescan()) else {
            return;
        };
        info!("{path} is not in Navidrome yet; asking it to scan again");
        if AssertUnwindSafe(rescan()).catch_unwind().await.is_err() {
            debug!("Rescan for {path} failed");
        }
    }

    fn resolve_rescan(&self) -> Option<Rescan> {
        let library = self.services.as_ref()?.library.clone();
        Some(Arc::new(move || {
            let library = library.clone();
            async move {
                library.trigger_library_scan(true).await;
            }
            .boxed()
        }))
    }

    fn resolve_lookup(&self) -> Option<LibraryLookup> {
        if let Some(lookup) = self.watch.lock().library_lookup.clone() {
            return Some(lookup);
        }
        let services = self.services.as_ref()?;
        // Without an admin identity the resolver can never answer, so waiting would only hold
        // the ring on "importing" for minutes for nothing.
        services.identity.get_scan_auth()?;
        let resolver = services.resolver.clone();
        Some(Arc::new(move |artist, title, path| {
            let resolver = resolver.clone();
            async move { Ok(resolver.find_id_by_path(&artist, &title, &path).await) }.boxed()
        }))
    }
}

/// The entry for this song, made or restarted as needed. Caller holds the lock.
fn open<'a>(
    state: &'a mut State,
    provider: &str,
    external_id: &str,
    restart_finished: bool,
    now: DateTime<Utc>,
) -> &'a mut Entry {
    let key = key_of(provider, external_id);
    state.seq += 1;
    let seq = state.seq;
    if let Some(entry) = state.entries.get(&key) {
        if !(restart_finished && entry.finished()) {
            // Not restarted: the sequence number taken above was not used.
            state.seq -= 1;
            return state.entries.get_mut(&key).expect("checked above");
        }
        let entry = state.entries.get_mut(&key).expect("checked above");
        entry.run += 1;
        entry.owners.clear();
        entry.source = None;
        entry.progress = None;
        entry.bytes_done = None;
        entry.bytes_total = None;
        entry.error = None;
        entry.library_id = None;
        entry.note = None;
    } else {
        state.entries.set(
            key.clone(),
            Entry {
                provider: to_lower_invariant(provider.trim()),
                external_id: external_id.to_string(),
                client_id: None,
                artist: None,
                title: None,
                album: None,
                owners: Vec::new(),
                source: None,
                state: AcquisitionState::Queued,
                progress: None,
                bytes_done: None,
                bytes_total: None,
                started_at: now,
                updated_at: now,
                error: None,
                library_id: None,
                note: None,
                seq,
                run: 0,
            },
        );
    }
    let entry = state.entries.get_mut(&key).expect("made or restarted above");
    entry.state = AcquisitionState::Queued;
    entry.seq = seq;
    entry.started_at = now;
    entry.updated_at = now;
    entry
}

/// Caller holds the lock.
fn end_of(state: &State, key: &str) -> Option<AcquisitionEnd> {
    let entry = state.entries.get(key)?;
    let done = entry.state == AcquisitionState::Done;
    Some(AcquisitionEnd {
        key: key.to_string(),
        artist: entry.artist.clone(),
        title: entry.title.clone(),
        done,
        library_id: if done { entry.library_id.clone() } else { None },
        album_keys: state
            .albums
            .iter()
            .filter(|(_, claim)| claim.track_keys.iter().any(|k| k == key))
            .map(|(album, _)| album.clone())
            .collect(),
    })
}

/// Drop what has expired, then the oldest past the cap. Caller holds the lock.
fn prune(state: &mut State, now: DateTime<Utc>) {
    let expired: Vec<String> = state
        .entries
        .iter()
        .filter(|(_, entry)| {
            let age = now - entry.updated_at;
            if entry.finished() {
                age >= AcquisitionTracker::FINISHED_RETENTION
            } else {
                age >= AcquisitionTracker::STALLED_RETENTION
            }
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in expired {
        state.entries.remove(&key);
    }
    let stale: Vec<String> = state
        .albums
        .iter()
        .filter(|(_, claim)| now - claim.updated_at >= AcquisitionTracker::STALLED_RETENTION)
        .map(|(key, _)| key.clone())
        .collect();
    for key in stale {
        state.albums.remove(&key);
    }

    if state.entries.len() <= AcquisitionTracker::CAPACITY {
        return;
    }
    // Finished rows go first: a running download is the one somebody is watching.
    let mut order: Vec<(bool, DateTime<Utc>, String)> = state
        .entries
        .iter()
        .map(|(key, entry)| (!entry.finished(), entry.updated_at, key.clone()))
        .collect();
    order.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let excess = state.entries.len() - AcquisitionTracker::CAPACITY;
    for (_, _, key) in order.into_iter().take(excess) {
        state.entries.remove(&key);
    }
}

fn snapshot(entry: &Entry, ahead: Option<i32>) -> AcquisitionSnapshot {
    let mut owners = entry.owners.clone();
    owners.sort_by(|a, b| compare_ordinal_ignore_case(a, b));
    AcquisitionSnapshot {
        id: entry
            .client_id
            .clone()
            .unwrap_or_else(|| entry.external_id.clone()),
        provider: entry.provider.clone(),
        external_id: entry.external_id.clone(),
        artist: entry.artist.clone(),
        title: entry.title.clone(),
        album: entry.album.clone(),
        requested_by: owners,
        source: entry.source.clone(),
        state: entry.state,
        progress: entry.progress,
        bytes_done: entry.bytes_done,
        bytes_total: entry.bytes_total,
        started_at: entry.started_at,
        updated_at: entry.updated_at,
        error: entry.error.clone(),
        library_id: entry.library_id.clone(),
        ahead,
        note: entry.note.clone(),
    }
}

/// How many downloads, anyone's, are ahead of a queued one. Octo fetches one song at a
/// time, so every running entry that arrived first is in front of it. Only a count leaves
/// here, never whose they are. `None` once it is past the queue.
fn ahead_of(entry: &Entry, running: &[(i64, &Entry)]) -> Option<i32> {
    (entry.state == AcquisitionState::Queued).then(|| {
        running
            .iter()
            .filter(|(seq, other)| !std::ptr::eq(*other, entry) && *seq < entry.seq)
            .count() as i32
    })
}

#[cfg(test)]
#[path = "acquisition_tracker_tests.rs"]
mod tests;
