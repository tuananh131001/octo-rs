//! Port of `Services/Library/LibraryActionExecutor.cs`: `LibraryActionRequest`,
//! `LibraryActionOutcome`, `LibraryActionCodes` and the executor.
//!
//! The C# `ApplyAsync` took a `CancellationToken` that only reached the lookups before the file
//! was touched; here [`LibraryActionExecutor::apply`] takes none, and the workers stop between
//! actions instead (see known-diffs.md). Where the C# let an exception out of `ApplyAsync` (the
//! mappings file could not be saved after a delete), `apply` answers `Err`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::lidarr::lidarr_client::LidarrError;
use octo_core::settings::{DownloadSource, LibraryAction, SettingsStore};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use octo_media::audio::spectrum_analyzer::{SpectrumAnalyzer, SpectrumReport};
use octo_media::tags::{KeptIdentity, KeptIdentityTags};
use octo_subsonic::SubsonicCredential;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use super::library_action_journal::{
    LibraryActionEntry, LibraryActionJournal, LibraryActionState, action_name,
};
use super::library_action_quarantine::LibraryActionQuarantine;
use super::navidrome_song_path_resolver::get_full_path;
use super::notice_queue::NoticeQueue;
use super::replacement_handoff::{
    BeforeReveal, OnRevealed, ReplacementHandoff, ReplacementRejectedException,
};
use super::upgrade_sources::UpgradeSources;
use super::{NavidromeSongPathResolver, ResolvedSongFile};
use crate::services::common::{StarOnArrival, TrackAcquisitionQueue};
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::soulseek_link::{OFFLINE_TEXT, SoulseekLinkState};
use crate::services::soulseek::{
    ExternalIdRegistry, ISoulseekLink, RejectedPeerRegistry, SoulseekMetadataService,
};

/// Told the provider and external id of the replacement download the moment it is queued, so the
/// upgrade queue can follow that download's progress.
pub type OnReplacementQueued = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// One request for a library action.
#[derive(Clone)]
pub struct LibraryActionRequest {
    pub action: LibraryAction,
    pub navidrome_id: String,
    pub username: String,
    pub credential: Option<SubsonicCredential>,
    pub on_replacement_queued: Option<OnReplacementQueued>,
}

impl LibraryActionRequest {
    pub fn new(action: LibraryAction, navidrome_id: impl Into<String>, username: impl Into<String>) -> Self {
        LibraryActionRequest {
            action,
            navidrome_id: navidrome_id.into(),
            username: username.into(),
            credential: None,
            on_replacement_queued: None,
        }
    }

    pub fn with_credential(mut self, credential: Option<SubsonicCredential>) -> Self {
        self.credential = credential;
        self
    }
}

/// What happened to one library action request.
///
/// Code says WHY, for callers that act on the reason: the upgrade queue waits for Soulseek on
/// one and reports "no FLAC found" on the other. Detail stays the words for people.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryActionOutcome {
    pub state: LibraryActionState,
    pub detail: Option<String>,
    pub code: Option<String>,

    /// For a replacement that went in: the new file, so a caller can say what it is.
    pub new_path: Option<String>,

    /// For a replacement that went in: where the original waits in quarantine.
    pub quarantine_path: Option<String>,
}

impl LibraryActionOutcome {
    pub fn new(state: LibraryActionState, detail: Option<String>) -> Self {
        Self {
            state,
            detail,
            ..Default::default()
        }
    }

    fn said(state: LibraryActionState, detail: impl Into<String>) -> Self {
        Self::new(state, Some(detail.into()))
    }

    fn coded(state: LibraryActionState, detail: impl Into<String>, code: Option<&str>) -> Self {
        Self {
            code: code.map(str::to_string),
            ..Self::said(state, detail)
        }
    }

    /// Whether the request has been consumed. Anything else leaves the track in the playlist
    /// and the rating set, so a request is never silently swallowed and retries if the
    /// operator fixes whatever blocked it.
    pub fn consumed(&self) -> bool {
        matches!(
            self.state,
            LibraryActionState::Applied | LibraryActionState::Skipped
        )
    }
}

/// `LibraryActionCodes`.
pub struct LibraryActionCodes;

impl LibraryActionCodes {
    /// slskd is not logged in to Soulseek, so a replacement could only fail.
    pub const SOULSEEK_OFFLINE: &'static str = "soulseekOffline";

    /// The search found no copy good enough to replace the song with.
    pub const NO_REPLACEMENT: &'static str = "noReplacement";
}

/// Navidrome shows this id at this file. Tests set it; otherwise the resolver.
pub type ShowsAt = Arc<dyn Fn(String, String) -> BoxFuture<'static, bool> + Send + Sync>;

/// Asks Navidrome for a forced scan. Tests set it; otherwise the library service.
pub type ForceScan = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

/// The C# internal settable properties: `HistoryPoll`, `HistoryAttempts`, `ShowsAt`, `ForceScan`.
#[derive(Clone)]
pub struct ExecutorSeams {
    pub history_poll: Duration,
    pub history_attempts: i32,
    pub shows_at: Option<ShowsAt>,
    pub force_scan: Option<ForceScan>,
}

impl Default for ExecutorSeams {
    fn default() -> Self {
        ExecutorSeams {
            history_poll: Duration::from_secs(15),
            history_attempts: 40,
            shows_at: None,
            force_scan: None,
        }
    }
}

/// What the executor is built from: the C# constructor's parameters, the optional ones last.
pub struct LibraryActionExecutorParts {
    pub resolver: Arc<NavidromeSongPathResolver>,
    pub quarantine: Arc<LibraryActionQuarantine>,
    pub journal: Arc<LibraryActionJournal>,
    pub library: Arc<dyn ILocalLibraryService>,
    pub ids: Arc<ExternalIdRegistry>,
    pub rejected_peers: Arc<RejectedPeerRegistry>,
    pub acquisitions: Arc<TrackAcquisitionQueue>,
    /// `IOptionsMonitor` of the library action, Soulseek and Subsonic settings: read at use.
    pub settings: Arc<SettingsStore>,
    pub notices: Option<Arc<NoticeQueue>>,
    pub spectrum: Option<Arc<SpectrumAnalyzer>>,
    pub stars: Option<Arc<StarOnArrival>>,
    pub soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    pub sources: Option<Arc<UpgradeSources>>,
}

/// How a replacement ended: the outcome, where the original waits, and the new file.
struct Replaced {
    outcome: LibraryActionOutcome,
    quarantine_path: Option<String>,
    new_path: Option<String>,
}

impl Replaced {
    fn failed(detail: impl Into<String>) -> Self {
        Replaced {
            outcome: LibraryActionOutcome::said(LibraryActionState::Failed, detail),
            quarantine_path: None,
            new_path: None,
        }
    }
}

/// Applies one library action. The only code in this feature that touches a file.
///
/// Every path through here either proves what it is acting on or does nothing. The resolver
/// supplies the proof, the quarantine makes a wrong answer recoverable, and the journal makes
/// a half-finished action reconcilable rather than repeatable.
pub struct LibraryActionExecutor {
    resolver: Arc<NavidromeSongPathResolver>,
    quarantine: Arc<LibraryActionQuarantine>,
    journal: Arc<LibraryActionJournal>,
    library: Arc<dyn ILocalLibraryService>,
    ids: Arc<ExternalIdRegistry>,
    rejected_peers: Arc<RejectedPeerRegistry>,
    acquisitions: Arc<TrackAcquisitionQueue>,
    settings: Arc<SettingsStore>,
    notices: Option<Arc<NoticeQueue>>,
    spectrum: Option<Arc<SpectrumAnalyzer>>,
    stars: Option<Arc<StarOnArrival>>,
    soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    sources: Option<Arc<UpgradeSources>>,
    reconciled: AtomicBool,

    // The songs a library action is working on right now, by the original's full path. The
    // rating, playlist and weekly upgrade workers all call apply, and two replacements of
    // one song at once share one download: the second could quarantine or delete what the
    // first had just put in place. A set rather than a lock per song, because nobody waits for
    // it (the second action is skipped), so an entry can simply be removed when the action ends.
    // Paths compare ordinally, as on every platform but Windows.
    busy: Mutex<HashSet<String>>,
    seams: Mutex<ExecutorSeams>,
}

/// Takes a song out of the busy set when the action on it ends, however it ends.
struct BusyGuard<'a> {
    busy: &'a Mutex<HashSet<String>>,
    key: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.busy.lock().remove(&self.key);
    }
}

/// The C# exception types the replacement loop tells apart.
fn is_rejected(error: &anyhow::Error) -> Option<&ReplacementRejectedException> {
    error.downcast_ref::<ReplacementRejectedException>()
}

/// `FileNotFoundException`: a search that found nothing usable. A download reports it as an
/// `io::Error` of kind `NotFound`, or Lidarr's `FileNotFound`.
fn is_file_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        || matches!(
            error.downcast_ref::<LidarrError>(),
            Some(LidarrError::FileNotFound(_))
        )
}

/// `InvalidOperationException`, as Lidarr's client and fetcher report it.
fn is_invalid_operation(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<LidarrError>(),
        Some(LidarrError::InvalidOperation(_))
    )
}

/// `Path.GetFileName`.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// `Path.GetExtension`: the name's last dot onward, empty when there is none or it ends there.
fn extension(path: &str) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

/// `File.GetLastWriteTimeUtc(path).Ticks`: 1601-01-01 for a file that is not there.
fn last_write_ticks(path: &str) -> i64 {
    const MISSING: i64 = 504_911_232_000_000_000;
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|modified| dotnet_ticks(chrono::DateTime::<Utc>::from(modified)))
        .unwrap_or(MISSING)
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn try_delete(path: &str) {
    if file_exists(path) {
        // Best effort.
        let _ = std::fs::remove_file(path);
    }
}

fn describe(action: LibraryAction) -> &'static str {
    match action {
        LibraryAction::Delete => "remove",
        LibraryAction::WrongSong => "replace (wrong song)",
        LibraryAction::WrongVersion => "replace (wrong version)",
        LibraryAction::BetterQuality => "upgrade",
        LibraryAction::Keep => "keep",
    }
}

impl LibraryActionExecutor {
    pub const BUSY_TEXT: &'static str =
        "Another action on this song is still running, so this one was skipped.";
    pub const JOINED_TEXT: &'static str =
        "Another download of this song was already running, so nothing changed.";
    pub const HISTORY_KEPT_TEXT: &'static str =
        "Navidrome kept it as the same song, so its plays, favorites and playlist places stayed with it.";
    pub const HISTORY_LOST_TEXT: &'static str =
        "Navidrome took it for a new song, so its plays and playlist places stayed with the old entry.";

    pub fn new(parts: LibraryActionExecutorParts) -> Self {
        LibraryActionExecutor {
            resolver: parts.resolver,
            quarantine: parts.quarantine,
            journal: parts.journal,
            library: parts.library,
            ids: parts.ids,
            rejected_peers: parts.rejected_peers,
            acquisitions: parts.acquisitions,
            settings: parts.settings,
            notices: parts.notices,
            spectrum: parts.spectrum,
            stars: parts.stars,
            soulseek_link: parts.soulseek_link,
            sources: parts.sources,
            reconciled: AtomicBool::new(false),
            busy: Mutex::new(HashSet::new()),
            seams: Mutex::new(ExecutorSeams::default()),
        }
    }

    /// Sets the test seams.
    pub fn configure(&self, change: impl FnOnce(&mut ExecutorSeams)) {
        change(&mut self.seams.lock());
    }

    pub fn journal(&self) -> &Arc<LibraryActionJournal> {
        &self.journal
    }

    pub async fn apply(
        self: &Arc<Self>,
        request: LibraryActionRequest,
    ) -> anyhow::Result<LibraryActionOutcome> {
        let settings = self.settings.current().library_actions.clone();

        if !settings.enabled {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "Library actions are off.",
            ));
        }

        // Reconcile once per process before the first action. The playlist worker does this at
        // startup, but an install with only star ratings enabled never starts that worker, so an
        // interrupted action there was never looked at again.
        if !self.reconciled.swap(true, Ordering::SeqCst) {
            let quarantine = self.quarantine.clone();
            self.journal
                .reconcile(Some(&move |path: &str| quarantine.restore(path).moved));
        }
        if !settings.is_allowed(Some(&request.username)) {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                format!("{} is not on the allowlist.", request.username),
            ));
        }

        let enabled = settings
            .effective_actions()
            .into_iter()
            .any(|entry| entry.action == request.action && entry.enabled);
        if !enabled {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                format!("{} is not enabled.", action_name(request.action)),
            ));
        }

        // Keep is an answer, not an operation. It touches no file, so there is nothing to rehearse
        // and nothing to prove about which file it is.
        if request.action == LibraryAction::Keep {
            let kept = self
                .notices
                .as_ref()
                .and_then(|notices| notices.mark_kept(&request.username, &request.navidrome_id));
            // Not something Octo asked about: nothing to answer and nothing to record. Five stars
            // on any other track is just a rating, and the journal holds real actions.
            let Some(kept) = kept else {
                return Ok(LibraryActionOutcome::said(
                    LibraryActionState::Skipped,
                    "Octo had not asked about this track.",
                ));
            };

            const KEPT_DETAIL: &str = "Kept. Octo will not ask about this track again.";
            self.journal.record(LibraryActionEntry {
                key: LibraryActionJournal::make_key(
                    request.action,
                    &request.navidrome_id,
                    &format!("keep:{}", dotnet_ticks(Utc::now())),
                ),
                title: kept.title.clone(),
                artist: kept.artist.clone(),
                album: kept.album.clone().unwrap_or_default(),
                ..Self::entry(
                    &request,
                    None,
                    LibraryActionState::Applied,
                    Some(KEPT_DETAIL),
                    false,
                )
            });
            info!("{} kept '{} - {}'", request.username, kept.artist, kept.title);
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Applied,
                KEPT_DETAIL,
            ));
        }

        let Some(resolved) = self.resolver.resolve(&request.navidrome_id).await else {
            // Never a success and never a delete. The request stays where it is, so fixing a
            // mount and waiting makes it work rather than requiring the user to ask again.
            let detail = "Could not work out which file this is, so nothing was touched.";
            self.journal.record(Self::entry(
                &request,
                None,
                LibraryActionState::Unresolved,
                Some(detail),
                settings.dry_run,
            ));
            return Ok(LibraryActionOutcome::said(LibraryActionState::Unresolved, detail));
        };

        let fingerprint =
            LibraryActionJournal::fingerprint(resolved.size_bytes, last_write_ticks(&resolved.absolute_path));
        if self
            .journal
            .already_applied(request.action, &request.navidrome_id, &fingerprint)
        {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "Already done.",
            ));
        }

        let key = LibraryActionJournal::make_key(request.action, &request.navidrome_id, &fingerprint);
        let pending = LibraryActionEntry {
            key: key.clone(),
            ..Self::entry(
                &request,
                Some(&resolved),
                LibraryActionState::Pending,
                None,
                settings.dry_run,
            )
        };

        if settings.dry_run {
            let detail = format!(
                "Dry run: would {} {}",
                describe(request.action),
                resolved.absolute_path
            );
            self.journal.record(LibraryActionEntry {
                state: LibraryActionState::Rehearsed,
                detail: Some(detail.clone()),
                ..pending
            });
            info!(
                "Library action {} by {} (DRY RUN): would {} {}",
                action_name(request.action),
                request.username,
                describe(request.action),
                resolved.absolute_path
            );
            // Deliberately NOT consumed: the same set replays every cycle so the operator sees
            // a stable list rather than a rehearsal that quietly emptied the playlist.
            return Ok(LibraryActionOutcome::said(LibraryActionState::Rehearsed, detail));
        }

        // Better quality with every source out could only fail, and before this it was journaled
        // as "no FLAC found" every sweep. Not consumed, so the request stays and runs once slskd
        // is back. Nothing is written and no file is touched. With Lidarr also set up, a Soulseek
        // outage just means Lidarr alone is asked.
        if request.action == LibraryAction::BetterQuality {
            let sources = self.sources_for(request.action).await;
            if sources.is_empty() && self.waiting_for_soulseek().await {
                return Ok(LibraryActionOutcome::coded(
                    LibraryActionState::Failed,
                    OFFLINE_TEXT,
                    Some(LibraryActionCodes::SOULSEEK_OFFLINE),
                ));
            }
            if sources.is_empty() {
                return Ok(LibraryActionOutcome::said(
                    LibraryActionState::Failed,
                    "Better quality has no source set up: it needs slskd or Lidarr.",
                ));
            }
        }

        // Taken before the Pending entry is written: a second action on the same file content
        // has the same journal key and would overwrite the first one's entry.
        let song_key = get_full_path(&resolved.absolute_path);
        if !self.busy.lock().insert(song_key.clone()) {
            self.journal.record(LibraryActionEntry {
                key: LibraryActionJournal::make_key(
                    request.action,
                    &request.navidrome_id,
                    &format!("busy:{}", dotnet_ticks(Utc::now())),
                ),
                ..Self::entry(
                    &request,
                    Some(&resolved),
                    LibraryActionState::Skipped,
                    Some(Self::BUSY_TEXT),
                    false,
                )
            });
            info!(
                "Library action {} by {} skipped: another action on {} is still running",
                action_name(request.action),
                request.username,
                resolved.absolute_path
            );
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                Self::BUSY_TEXT,
            ));
        }
        let _busy = BusyGuard {
            busy: &self.busy,
            key: song_key,
        };
        self.apply_on_file(&request, &resolved, &key, pending).await
    }

    async fn apply_on_file(
        self: &Arc<Self>,
        request: &LibraryActionRequest,
        resolved: &ResolvedSongFile,
        key: &str,
        pending: LibraryActionEntry,
    ) -> anyhow::Result<LibraryActionOutcome> {
        // Read now, while Navidrome still knows the song here. Only the acting user's own
        // favorite can be read, with their own sign-in; anyone else's is out of reach.
        let carry_star = self.was_starred_by_requester(request).await;

        // Written BEFORE the file is touched. A crash between the two leaves this Pending, and
        // startup reconciles it against the filesystem rather than blindly re-running.
        self.journal.record(pending);
        if !self.journal.flush() {
            // The whole safety story rests on this entry being on disk before the file moves.
            const UNRECORDED: &str = "Could not write the action journal, so the file was not touched.";
            self.journal.complete(
                key,
                LibraryActionState::Failed,
                Some(UNRECORDED),
                None,
                None,
                None,
            );
            return Ok(LibraryActionOutcome::said(LibraryActionState::Failed, UNRECORDED));
        }

        let music_root = self.resolver.music_root();
        let outcome;
        let mut replaced: Option<Replaced> = None;
        let quarantine_path: Option<String>;
        if request.action == LibraryAction::Delete {
            let moved = self
                .quarantine
                .move_file(resolved, &music_root, request.action, &request.username);
            if !moved.moved {
                self.journal.complete(
                    key,
                    LibraryActionState::Failed,
                    moved.error.as_deref(),
                    None,
                    None,
                    None,
                );
                return Ok(LibraryActionOutcome::new(LibraryActionState::Failed, moved.error));
            }
            quarantine_path = moved.quarantine_path;
            self.journal.complete(
                key,
                LibraryActionState::Pending,
                Some("Moved to quarantine; finishing."),
                quarantine_path.as_deref(),
                None,
                None,
            );
            self.journal.flush();
            self.library.forget_mapping(&resolved.absolute_path).await?;
            outcome = LibraryActionOutcome::said(
                LibraryActionState::Applied,
                "Removed. It will not be downloaded again.",
            );
        } else {
            // The original stays in place, playable, until its replacement has passed; the two
            // swap places in one moment (W8).
            let done = self.reacquire(request, resolved, key, &music_root).await;
            outcome = done.outcome.clone();
            quarantine_path = done.quarantine_path.clone();
            replaced = Some(done);
        }

        self.journal.complete(
            key,
            outcome.state,
            outcome.detail.as_deref(),
            quarantine_path.as_deref(),
            None,
            None,
        );
        if outcome.state == LibraryActionState::Applied
            && let Some(notices) = &self.notices
        {
            notices.mark_acted(&request.navidrome_id);
        }
        self.journal.flush();
        let new_path = replaced.and_then(|r| r.new_path);
        // Off this call, which the rating and playlist workers wait on: it can take ten minutes.
        if outcome.state == LibraryActionState::Applied
            && let Some(new_path) = new_path.clone()
        {
            let executor = Arc::clone(self);
            let (request, original, key) = (request.clone(), resolved.clone(), key.to_string());
            tokio::spawn(async move {
                executor
                    .confirm_history_kept(&request, &original, &new_path, &key, carry_star)
                    .await;
            });
        }
        Ok(LibraryActionOutcome {
            new_path,
            quarantine_path,
            ..outcome
        })
    }

    async fn reacquire(
        self: &Arc<Self>,
        request: &LibraryActionRequest,
        original: &ResolvedSongFile,
        key: &str,
        music_root: &str,
    ) -> Replaced {
        if request.action == LibraryAction::WrongSong {
            self.blacklist_source(original, request).await;
        }

        // Read while the original is in place: Navidrome built this song's ids from these tags.
        let identity = KeptIdentityTags::read(
            Path::new(&original.absolute_path),
            original.album_artist.as_deref(),
        );
        // What the original looked like when the action started. A download takes minutes, and
        // whatever is at that path by the end may no longer be the file this action was about.
        let admission = Arc::new(Admission {
            executor: Arc::clone(self),
            action: request.action,
            username: request.username.clone(),
            original: original.clone(),
            key: key.to_string(),
            music_root: music_root.to_string(),
            started_last_write: last_write_ticks(&original.absolute_path),
            quarantine_path: Mutex::new(None),
        });
        // What each source tried before the last one said, for the words when all of them miss.
        let mut misses: Vec<String> = Vec::new();
        let earlier = |misses: &[String]| {
            if misses.is_empty() {
                String::new()
            } else {
                format!(" Before that, {}.", misses.join("; "))
            }
        };

        let mut handoff: Option<Arc<ReplacementHandoff>> = None;
        let attempt: Result<Option<String>, Arc<anyhow::Error>> = async {
            let routing = SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(original.artist.clone()),
                title: Some(original.title.clone()),
                album: Some(original.album.clone()),
                duration: original.duration_seconds,
                ..Default::default()
            };
            let external_id = self.ids.register(routing);
            if let Some(listener) = &request.on_replacement_queued {
                let told = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    listener(SoulseekMetadataService::PROVIDER_NAME, &external_id)
                }));
                if told.is_err() {
                    debug!("Replacement listener failed: it panicked");
                }
            }
            if identity.is_none() {
                warn!(
                    "Library action {}: could not read the tags of {}, so Navidrome will treat its replacement as a new song",
                    action_name(request.action),
                    original.absolute_path
                );
            }
            handoff = identity
                .clone()
                .map(|identity| Arc::new(admission.clone().handoff(identity)));
            // DownloadSongInternalAsync hands back a file it has a mapping for instead of
            // fetching. The handoff skips that, so only a download without one needs this.
            if handoff.is_none() {
                self.library
                    .forget_mapping(&original.absolute_path)
                    .await
                    .map_err(Arc::new)?;
            }

            // In order, and on to the next when one finds nothing or its copy fails the checks:
            // Soulseek first, then Lidarr, for Better quality with both set up.
            let sources = self.sources_for(request.action).await;
            let upgrade_search = request.action == LibraryAction::BetterQuality;
            let requested_by = self
                .settings
                .current()
                .subsonic
                .record_requested_by
                .then_some(request.username.as_str());
            let mut attempt = 0;
            loop {
                let Some(&source) = sources.get(attempt) else {
                    return Err(Arc::new(anyhow::anyhow!(
                        "Index was out of range. Must be non-negative and less than the size of the collection. (Parameter 'index')"
                    )));
                };
                let completion = self.acquisitions.enqueue(
                    SoulseekMetadataService::PROVIDER_NAME,
                    &external_id,
                    true,
                    false,
                    true,
                    source,
                    false,
                    requested_by,
                    upgrade_search,
                    handoff.clone(),
                );
                match completion.wait().await {
                    Ok(path) => return Ok(Some(path)),
                    Err(e)
                        if attempt + 1 < sources.len()
                            && handoff.as_ref().is_none_or(|h| h.revealed_path().is_none())
                            && (is_file_not_found(&e) || is_rejected(&e).is_some() || is_invalid_operation(&e)) =>
                    {
                        let why = is_rejected(&e)
                            .map(|rejected| rejected.problem.clone())
                            .unwrap_or_else(|| e.to_string());
                        let word = UpgradeSources::word(source.unwrap_or(DownloadSource::Soulseek));
                        misses.push(format!("{word}: {why}"));
                        info!(
                            "Library action {}: {word} had no copy of '{} - {}' ({why}); trying {}",
                            action_name(request.action),
                            original.artist,
                            original.title,
                            UpgradeSources::word(sources[attempt + 1].unwrap_or(DownloadSource::Soulseek))
                        );
                        attempt += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        .await;

        let quarantine_path = || admission.quarantine_path.lock().clone();
        let replacement = match attempt {
            Ok(replacement) => replacement.unwrap_or_default(),
            Err(e) => {
                if let Some(rejected) = is_rejected(&e) {
                    // Refused before it was ever in the library; the original never moved.
                    self.log_refused(request, original, &rejected.problem);
                    return Replaced::failed(format!(
                        "The replacement {}, so nothing changed.{}",
                        rejected.problem,
                        earlier(&misses)
                    ));
                }
                if let Some(revealed) = handoff.as_ref().and_then(|h| h.revealed_path()) {
                    // The replacement already took the original's place and only the bookkeeping
                    // after it failed. Putting the original back now would undo a finished swap.
                    warn!(
                        "Library action {}: the replacement for '{} - {}' is in place at {revealed}, but what followed failed: {e}",
                        action_name(request.action),
                        original.artist,
                        original.title
                    );
                    let quarantined = quarantine_path();
                    if !self.settings.current().library_actions.keep_replaced_originals
                        && let Some(path) = &quarantined
                    {
                        try_delete(path);
                    }
                    return Replaced {
                        outcome: LibraryActionOutcome::said(
                            LibraryActionState::Applied,
                            format!("Replaced with {}.", file_name(&revealed)),
                        ),
                        quarantine_path: quarantined,
                        new_path: Some(revealed),
                    };
                }
                // A search that found nothing usable reports not found, passed through the queue
                // unchanged, and the upgrade queue reports exactly that case as "no FLAC found".
                let Some(quarantined) = quarantine_path() else {
                    return Replaced {
                        outcome: LibraryActionOutcome::coded(
                            LibraryActionState::Failed,
                            format!(
                                "Could not find a replacement ({e}), so nothing changed.{}",
                                earlier(&misses)
                            ),
                            is_file_not_found(&e).then_some(LibraryActionCodes::NO_REPLACEMENT),
                        ),
                        quarantine_path: None,
                        new_path: None,
                    };
                };
                // Out, and nothing moved in: back to its exact path, whose row Navidrome still has.
                return Replaced {
                    outcome: self.restore_original(
                        &quarantined,
                        &format!("Could not finish the replacement ({e}), so nothing changed."),
                    ),
                    quarantine_path: Some(quarantined),
                    new_path: None,
                };
            }
        };

        // A handoff that was never used: this joined a download of the same song already in
        // flight, whose file belongs to whoever started it. Judging it here could quarantine
        // or delete a replacement that action had just put in place, so nothing is touched.
        if handoff.as_ref().is_some_and(|h| h.revealed_path().is_none()) {
            self.log_refused(request, original, "was another action's download");
            return Replaced::failed(Self::JOINED_TEXT);
        }
        // Without the handoff the file is already in the library under its own name: judged now.
        if handoff.is_none() {
            match admission.admit(&replacement).await {
                Ok(None) => {}
                Ok(Some(late)) => {
                    self.log_refused(request, original, &late);
                    if !replacement.is_empty() {
                        try_delete(&replacement);
                    }
                    return Replaced::failed(format!("The replacement {late}, so nothing changed."));
                }
                Err(e) => {
                    // What the C# caught as any other exception after the download.
                    return match quarantine_path() {
                        None => Replaced::failed(format!(
                            "Could not find a replacement ({e}), so nothing changed.{}",
                            earlier(&misses)
                        )),
                        Some(quarantined) => Replaced {
                            outcome: self.restore_original(
                                &quarantined,
                                &format!("Could not finish the replacement ({e}), so nothing changed."),
                            ),
                            quarantine_path: Some(quarantined),
                            new_path: None,
                        },
                    };
                }
            }
        }
        let quarantined = quarantine_path();
        if !self.settings.current().library_actions.keep_replaced_originals
            && let Some(path) = &quarantined
        {
            try_delete(path);
        }
        Replaced {
            outcome: LibraryActionOutcome::said(
                LibraryActionState::Applied,
                format!("Replaced with {}.", file_name(&replacement)),
            ),
            quarantine_path: quarantined,
            new_path: Some(replacement),
        }
    }

    /// The handoff a replacement download takes: the original's place, its identity, the check
    /// before the swap and the note right after it.
    pub fn handoff_for(
        original: &ResolvedSongFile,
        identity: KeptIdentity,
        admit: BeforeReveal,
        revealed: Option<OnRevealed>,
    ) -> ReplacementHandoff {
        ReplacementHandoff::new(original.absolute_path.clone(), identity, admit, revealed)
    }

    fn log_refused(&self, request: &LibraryActionRequest, original: &ResolvedSongFile, problem: &str) {
        info!(
            "Library action {}: the replacement for '{} - {}' {problem}; the original stays",
            action_name(request.action),
            original.artist,
            original.title
        );
    }

    /// One scan, then watch the ORIGINAL id until it shows the replacement (W8): proof that the
    /// plays, favorites and playlist places stayed with the song. About ten minutes, scanning
    /// again every two in case Navidrome was busy. If not shown, the rater's favorite (W6).
    pub async fn confirm_history_kept(
        &self,
        request: &LibraryActionRequest,
        original: &ResolvedSongFile,
        new_path: &str,
        key: &str,
        carry_star: bool,
    ) -> bool {
        let seams = self.seams.lock().clone();
        let mut kept = false;
        let mut attempt = 0;
        while attempt < seams.history_attempts.max(1) && !kept {
            if attempt % 8 == 0 {
                match &seams.force_scan {
                    Some(scan) => {
                        scan().await;
                    }
                    None => {
                        self.library.trigger_library_scan(true).await;
                    }
                }
            }
            tokio::time::sleep(seams.history_poll).await;
            kept = match &seams.shows_at {
                Some(shows_at) => shows_at(original.navidrome_id.clone(), new_path.to_string()).await,
                None => self.resolver.shows_at(&original.navidrome_id, new_path).await,
            };
            attempt += 1;
        }

        self.journal.complete(
            key,
            LibraryActionState::Applied,
            Some(&format!(
                "Replaced with {}. {}",
                file_name(new_path),
                if kept {
                    Self::HISTORY_KEPT_TEXT
                } else {
                    Self::HISTORY_LOST_TEXT
                }
            )),
            None,
            Some(kept),
            None,
        );
        self.journal.flush();
        if kept {
            info!(
                "Navidrome kept '{} - {}' as the same song after {}",
                original.artist,
                original.title,
                action_name(request.action)
            );
            return true;
        }
        warn!(
            "Navidrome did not keep '{} - {}' as the same song after {}; its plays and playlist places stay with the old entry",
            original.artist,
            original.title,
            action_name(request.action)
        );
        if carry_star && let (Some(stars), Some(credential)) = (&self.stars, &request.credential) {
            stars.star_when_visible(
                credential,
                &request.username,
                &original.artist,
                &original.title,
                new_path,
            );
        }
        false
    }

    /// Where a replacement may come from, in the order tried. Better quality uses the upgrade
    /// sources that are set up and can search now (never YouTube, whose MP3 a lossless check
    /// refuses anyway), and searches the slow way, because nobody is waiting (#70). Wrong song and
    /// wrong version use the download source, except that Lidarr there means Lidarr too: before,
    /// those replacements went to Soulseek whatever was configured. A None entry is the default.
    pub async fn sources_for(&self, action: LibraryAction) -> Vec<Option<DownloadSource>> {
        if action == LibraryAction::BetterQuality {
            // Without the plan (hosts that build the executor by hand): Soulseek, as it always was.
            let Some(sources) = &self.sources else {
                return if self.soulseek_out().await {
                    Vec::new()
                } else {
                    vec![Some(DownloadSource::Soulseek)]
                };
            };
            return sources.available().await.into_iter().map(Some).collect();
        }
        let lidarr_hearts = self.settings.current().subsonic.download_source == DownloadSource::Lidarr
            && self
                .sources
                .as_ref()
                .is_some_and(|s| s.plan().contains(&DownloadSource::Lidarr));
        if lidarr_hearts {
            vec![Some(DownloadSource::Lidarr), None]
        } else {
            vec![None]
        }
    }

    async fn waiting_for_soulseek(&self) -> bool {
        match &self.sources {
            Some(sources) => sources.waiting_for_soulseek().await,
            None => self.soulseek_out().await,
        }
    }

    async fn soulseek_out(&self) -> bool {
        match &self.soulseek_link {
            Some(link) => link
                .read(false)
                .await
                .is_some_and(|reading| reading.link == SoulseekLinkState::NotLoggedIn),
            None => false,
        }
    }

    /// Whether the person asking for a replacement had favorited the song. False when nothing
    /// will replace it, when the request carries no sign-in (playlist actions never do), or when
    /// Navidrome cannot say.
    pub async fn was_starred_by_requester(&self, request: &LibraryActionRequest) -> bool {
        if !matches!(
            request.action,
            LibraryAction::WrongSong | LibraryAction::WrongVersion | LibraryAction::BetterQuality
        ) {
            return false;
        }
        let (Some(credential), Some(stars)) = (&request.credential, &self.stars) else {
            return false;
        };
        stars.is_starred(credential, &request.navidrome_id).await
    }

    /// Why a replacement is not good enough to keep, or None when it is.
    fn unacceptable(action: LibraryAction, path: &str, original: &ResolvedSongFile) -> Option<String> {
        if path.is_empty() || !file_exists(path) {
            return Some("never arrived".into());
        }

        let length = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
        if length == 0 {
            return Some("was empty".into());
        }

        if action != LibraryAction::BetterQuality {
            return None;
        }

        // Better quality has to actually be better, or the action quietly downgrades a library.
        const LOSSLESS: [&str; 6] = [".flac", ".wav", ".aiff", ".aif", ".alac", ".ape"];
        let ext = extension(path);
        if !LOSSLESS
            .iter()
            .any(|l| octo_core::common::dotnet::eq_ignore_case(l, &ext))
        {
            return Some("is not lossless".into());
        }
        if length <= original.size_bytes {
            return Some("is no larger than the original".into());
        }

        None
    }

    /// Better quality means genuinely lossless: a FLAC made from an MP3 is no upgrade over the MP3
    /// it replaces, and over a real FLAC it is a downgrade. None when the spectrum says nothing
    /// against it, which includes the check being off or unable to run.
    pub fn not_really_lossless(report: &SpectrumReport) -> Option<String> {
        report
            .is_likely_lossy()
            .then(|| format!("is {}", report.describe()))
    }

    async fn spectrum_of(&self, path: &str) -> SpectrumReport {
        let settings = self.settings.current().soulseek.clone();
        match &self.spectrum {
            Some(spectrum) if settings.detect_transcodes => {
                spectrum
                    .analyze(
                        Path::new(path),
                        settings.effective_transcode_check_timeout_seconds(),
                    )
                    .await
            }
            _ => SpectrumReport::unknown("not checked", 0),
        }
    }

    fn restore_original(&self, quarantine_path: &str, detail: &str) -> LibraryActionOutcome {
        let restored = self.quarantine.restore(quarantine_path);
        if restored.moved {
            LibraryActionOutcome::said(LibraryActionState::Failed, detail)
        } else {
            LibraryActionOutcome::said(
                LibraryActionState::Failed,
                format!(
                    "{detail} The original could not be put back automatically and is in the quarantine folder."
                ),
            )
        }
    }

    /// Remember the peer that delivered a wrong file, so the ranking never offers it again.
    ///
    /// Only possible when the download recorded who sent it, which Octo started doing alongside
    /// this feature. For a file downloaded before that, or one Octo never downloaded, there is
    /// nothing to blacklist and the re-acquire is an ordinary search.
    async fn blacklist_source(&self, original: &ResolvedSongFile, request: &LibraryActionRequest) {
        if !self.settings.current().soulseek.verify_downloads {
            return;
        }

        let mapping = self
            .library
            .find_mapping_by_tags(
                Some(&original.artist),
                Some(&original.title),
                Some(&original.album),
            )
            .await;

        let peer = mapping
            .as_ref()
            .and_then(|m| m.source_peer.clone())
            .filter(|p| !p.is_empty());
        let file = mapping
            .as_ref()
            .and_then(|m| m.source_file.clone())
            .filter(|f| !f.is_empty());
        let (Some(peer), Some(file)) = (peer, file) else {
            info!(
                "Library action Wrong song: no record of who sent '{} - {}', so the search runs again without a blacklist",
                original.artist, original.title
            );
            return;
        };

        self.rejected_peers.deny(
            Some(&peer),
            Some(&file),
            &format!("reported as the wrong song by {}", request.username),
            &format!("{} - {}", original.artist, original.title),
        );
    }

    fn entry(
        request: &LibraryActionRequest,
        resolved: Option<&ResolvedSongFile>,
        state: LibraryActionState,
        detail: Option<&str>,
        dry_run: bool,
    ) -> LibraryActionEntry {
        LibraryActionEntry {
            key: LibraryActionJournal::make_key(
                request.action,
                &request.navidrome_id,
                if resolved.is_none() {
                    "unresolved"
                } else {
                    "pending"
                },
            ),
            action: request.action,
            navidrome_id: request.navidrome_id.clone(),
            username: request.username.clone(),
            title: resolved.map(|r| r.title.clone()).unwrap_or_default(),
            artist: resolved.map(|r| r.artist.clone()).unwrap_or_default(),
            album: resolved.map(|r| r.album.clone()).unwrap_or_default(),
            source_path: resolved.map(|r| r.absolute_path.clone()),
            quarantine_path: None,
            resolution: resolved.map(|r| r.source),
            state,
            detail: detail.map(str::to_string),
            dry_run,
            at_utc: Utc::now(),
            history_kept: None,
            revealed_path: None,
        }
    }
}

/// What the C# `AdmitAsync` and `Revealed` local functions closed over: one replacement's
/// original, where it started, and where it went once it was moved out.
struct Admission {
    executor: Arc<LibraryActionExecutor>,
    action: LibraryAction,
    username: String,
    original: ResolvedSongFile,
    key: String,
    music_root: String,
    started_last_write: i64,
    quarantine_path: Mutex<Option<String>>,
}

impl Admission {
    /// The handoff over this admission's check and note.
    fn handoff(self: Arc<Self>, identity: KeptIdentity) -> ReplacementHandoff {
        let admitting = self.clone();
        let admit: BeforeReveal = Arc::new(move |candidate: String| {
            let admission = admitting.clone();
            async move {
                // The download cannot be told of a failure here, so it is logged and the swap
                // goes ahead: the original is already out (see known-diffs.md).
                admission.admit(&candidate).await.unwrap_or_else(|e| {
                    warn!("Library action: the original's mapping could not be forgotten: {e}");
                    None
                })
            }
            .boxed()
        });
        let revealing = self.clone();
        let revealed: OnRevealed = Arc::new(move |path: &str| revealing.revealed(path));
        LibraryActionExecutor::handoff_for(&self.original, identity, admit, Some(revealed))
    }

    /// Judges the new file and, only when it passes, moves the original out. Called by the
    /// download just before the replacement moves in, or by the executor for one that ran
    /// without the handoff, so a scan never sees the song missing for the length of a download.
    async fn admit(&self, candidate: &str) -> anyhow::Result<Option<String>> {
        let executor = &self.executor;
        let mut problem = LibraryActionExecutor::unacceptable(self.action, candidate, &self.original);
        if problem.is_none() && self.action == LibraryAction::BetterQuality {
            problem = LibraryActionExecutor::not_really_lossless(&executor.spectrum_of(candidate).await);
        }
        if problem.is_some() {
            return Ok(problem);
        }
        let now = std::fs::metadata(&self.original.absolute_path)
            .ok()
            .filter(|m| m.is_file());
        let changed = match now {
            None => true,
            Some(now) => {
                now.len() as i64 != self.original.size_bytes
                    || last_write_ticks(&self.original.absolute_path) != self.started_last_write
            }
        };
        if changed {
            return Ok(Some("found the original changed while it was downloading".into()));
        }
        let moved =
            executor
                .quarantine
                .move_file(&self.original, &self.music_root, self.action, &self.username);
        if !moved.moved {
            return Ok(Some(format!(
                "could not take the original's place ({})",
                moved.error.unwrap_or_default()
            )));
        }
        *self.quarantine_path.lock() = moved.quarantine_path.clone();
        // On disk before the replacement moves in: a crash in between puts the original back.
        executor.journal.complete(
            &self.key,
            LibraryActionState::Pending,
            Some("Moved to quarantine; finishing."),
            moved.quarantine_path.as_deref(),
            None,
            None,
        );
        executor.journal.flush();
        // Only now that the original is out: a refused replacement leaves it with its
        // mapping, and so with the record of who sent it.
        executor
            .library
            .forget_mapping(&self.original.absolute_path)
            .await?;
        Ok(None)
    }

    /// Written the moment the replacement is in the original's place, so a restart after it
    /// finds the swap done rather than putting the original back beside it.
    fn revealed(&self, path: &str) {
        let quarantine_path = self.quarantine_path.lock().clone();
        self.executor.journal.complete(
            &self.key,
            LibraryActionState::Pending,
            Some("Replacement placed; finishing."),
            quarantine_path.as_deref(),
            None,
            Some(path),
        );
        self.executor.journal.flush();
    }
}

#[cfg(test)]
#[path = "library_action_executor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "library_action_one_at_a_time_tests.rs"]
mod one_at_a_time_tests;
