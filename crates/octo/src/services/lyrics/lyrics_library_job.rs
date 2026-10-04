//! Port of `Services/Lyrics/LyricsLibraryJob.cs`: `LyricsLibraryStore` (`lyrics-library.json`)
//! and `LyricsLibraryWorker`, a singleton that is also a hosted queue worker, with its walk.
//! The lyrics page's steps are `lyrics_library_steps` (the C# partial class's other file), the
//! run's data is `octo_core::lyrics::lyrics_library_job`, and `LyricsUndoJournal`, also in that
//! C# file, is `lyrics_undo_journal`.

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use octo_core::common::dotnet::{eq_ignore_case, is_blank, is_null_or_white_space, ordinal_ignore_case_key};
use octo_core::lyrics::lyrics_library_job::{OCTO_DOWNLOADS, WHOLE_LIBRARY};
use octo_core::lyrics::lyrics_library_steps::{OCTO_RESTARTED, SERVICES_STOPPED_ANSWERING};
use octo_core::lyrics::{
    LyricsLibraryMode, LyricsLibraryRequest, LyricsLibraryRun, LyricsLibraryStatus, LyricsReviewEntry,
    LyricsText, LyricsTiming,
};
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::lyrics_choices::LyricsChoiceService;
use super::lyrics_sidecar_writer::{LyricsJob, LyricsSidecarWriter, LyricsWrite, LyricsWriteOutcome};
use super::lyrics_undo_journal::LyricsUndoJournal;
use crate::services::local::ILocalLibraryService;
use crate::services::state_file;

/// The run's progress, on disk, with the genre backfill's idiom: a dirty bit and a coalesced
/// flush through a temporary file, so a long walk is not one write per song and a torn write
/// never loses the cursor a resume depends on.
///
/// The C# store ran its own 10 s timer and flushed once more on `Dispose`; here that is
/// [`LyricsLibraryStore::run_flusher`], registered as a worker by the app state.
pub struct LyricsLibraryStore {
    path: Option<PathBuf>,
    run: Mutex<LyricsLibraryRun>,
    dirty: AtomicBool,
}

impl LyricsLibraryStore {
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
    pub const MAX_REVIEW: usize = 500;
    const MAX_ERRORS: usize = 20;

    /// `path` None (or blank) keeps the run in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.filter(|path| !is_blank(&path.to_string_lossy()));
        let run = path.as_deref().and_then(load).unwrap_or_default();
        Self {
            path,
            run: Mutex::new(run),
            dirty: AtomicBool::new(false),
        }
    }

    /// A copy of the run as it stands.
    pub fn current(&self) -> LyricsLibraryRun {
        self.run.lock().clone()
    }

    /// Reads the run without copying it (its queue can be thousands of paths).
    pub fn read<R>(&self, read: impl FnOnce(&LyricsLibraryRun) -> R) -> R {
        read(&self.run.lock())
    }

    pub fn update(&self, mutate: impl FnOnce(&mut LyricsLibraryRun)) {
        {
            let mut run = self.run.lock();
            mutate(&mut run);
            if run.errors.len() > Self::MAX_ERRORS {
                let excess = run.errors.len() - Self::MAX_ERRORS;
                run.errors.drain(..excess);
            }
            if run.review.len() > Self::MAX_REVIEW {
                let excess = run.review.len() - Self::MAX_REVIEW;
                run.review.drain(..excess);
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    pub fn replace(&self, run: LyricsLibraryRun) {
        *self.run.lock() = run;
        self.dirty.store(true, Ordering::SeqCst);
        self.flush();
    }

    /// Writes the run when it changed since the last write.
    pub fn flush(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let json = octo_core::json::to_string(&*self.run.lock());
        if let Err(error) = state_file::write_atomic(path, json.as_bytes()) {
            self.dirty.store(true, Ordering::SeqCst);
            warn!("library lyrics state could not be written: {error}");
        }
    }

    /// The C# flush timer and `Dispose`: flushes every [`Self::FLUSH_INTERVAL`] until
    /// cancelled, then once more.
    pub async fn run_flusher(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        if self.path.is_none() {
            return Ok(());
        }
        let store = self.clone();
        state_file::flush_every(Self::FLUSH_INTERVAL, stopping, Arc::new(move || store.flush())).await
    }
}

fn load(path: &Path) -> Option<LyricsLibraryRun> {
    if !path.is_file() {
        return None;
    }
    let read = state_file::read_all_text(path)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            serde_json::from_str::<Option<LyricsLibraryRun>>(&text).map_err(|error| error.to_string())
        });
    let mut run = match read {
        Ok(run) => run?,
        Err(error) => {
            warn!("library lyrics state could not be read: {error}");
            return None;
        }
    };
    // Never resumed by itself: a restart may be how someone stopped it.
    if run.status == LyricsLibraryStatus::Running {
        run.status = LyricsLibraryStatus::Interrupted;
        run.reason = Some(OCTO_RESTARTED.to_string());
    }
    Some(run)
}

/// A run stopped because Octo is shutting down: the C# `OperationCanceledException` on the
/// stopping token, which the worker's loop turns into an Interrupted run.
#[derive(Debug)]
pub(super) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The operation was canceled.")
    }
}

impl std::error::Error for Cancelled {}

/// What one song came to in a walk: the write (None after an error), the tags it was looked up
/// by (None when it never was), and the error.
type Processed = (Option<LyricsWrite>, Option<LyricsJob>, Option<String>);

/// "Find lyrics for the library": walks the songs that have no lyrics file and no lyrics in
/// their tags, and saves lyrics for each one the sources have (LYRICS_SAVE_TO says where). With
/// the upgrade box ticked, songs whose lyrics are weaker than the sources would choose are looked
/// up again too. One song at a time with a pause between, so a library of thousands never floods
/// a lyrics service; stoppable from the dashboard, and resumable after a stop or a restart from
/// where it was.
///
/// Where it writes follows the rule the downloads follow: beside the songs Octo downloaded, and
/// beside every library song only when "Write lyrics files beside all library songs" is on.
pub struct LyricsLibraryWorker {
    pub(super) store: Arc<LyricsLibraryStore>,
    pub(super) writer: Arc<LyricsSidecarWriter>,
    settings: Arc<SettingsStore>,
    /// Resolved from a scope when it is needed in C#; only asked for Octo's downloads.
    library: Arc<dyn ILocalLibraryService>,
    music_root: Box<dyn Fn() -> String + Send + Sync>,
    pub(super) journal: Arc<LyricsUndoJournal>,
    /// The pause after each song that needed a lookup ([`Self::GAP`]; the tests set none).
    pub(super) gap: Duration,
    pub(super) cancel_requested: AtomicBool,
    pending: AtomicBool,
    sender: mpsc::Sender<LyricsLibraryRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<LyricsLibraryRequest>>,
}

impl LyricsLibraryWorker {
    /// The pause after each song that needed a lookup.
    pub const GAP: Duration = Duration::from_millis(1500);

    /// When this many songs in a row find every service busy, the run stops rather than walk the
    /// rest of the library into the same wall; it can be resumed later.
    pub const BUSY_IN_A_ROW_LIMIT: i32 = 10;

    const AUDIO_EXTENSIONS: [&'static str; 15] = [
        ".mp3", ".flac", ".m4a", ".mp4", ".aac", ".ogg", ".oga", ".opus", ".wav", ".wma", ".aiff", ".aif",
        ".ape", ".wv", ".dsf",
    ];

    /// `music_root` is `NavidromeSongPathResolver.MusicRoot`, read at each whole-library run.
    pub fn new(
        store: Arc<LyricsLibraryStore>,
        writer: Arc<LyricsSidecarWriter>,
        settings: Arc<SettingsStore>,
        library: Arc<dyn ILocalLibraryService>,
        music_root: impl Fn() -> String + Send + Sync + 'static,
        journal: Arc<LyricsUndoJournal>,
    ) -> Self {
        // Capacity 1, and a request past it is dropped (`DropWrite`): `pending` keeps a second
        // one from ever being written while one waits.
        let (sender, receiver) = mpsc::channel(1);
        Self {
            store,
            writer,
            settings,
            library,
            music_root: Box::new(music_root),
            journal,
            gap: Self::GAP,
            cancel_requested: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
        }
    }

    /// The pause after each looked-up song (the C# tests set the static `Gap` to zero).
    pub fn with_gap(mut self, gap: Duration) -> Self {
        self.gap = gap;
        self
    }

    /// Whether Save wrote anything Undo can put back.
    pub fn can_undo(&self) -> bool {
        self.journal.has_entries()
    }

    pub fn current(&self) -> LyricsLibraryRun {
        self.store.current()
    }

    pub fn store(&self) -> &Arc<LyricsLibraryStore> {
        &self.store
    }

    pub fn is_running(&self) -> bool {
        self.store.read(|run| run.status == LyricsLibraryStatus::Running)
            || self.pending.load(Ordering::SeqCst)
    }

    /// False when a run is going or about to, which the dashboard shows as "already running".
    pub fn try_enqueue(&self, request: LyricsLibraryRequest) -> bool {
        if self.store.read(|run| run.status == LyricsLibraryStatus::Running)
            || self
                .pending
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return false;
        }
        if self.sender.try_send(request).is_ok() {
            return true;
        }
        self.pending.store(false, Ordering::SeqCst);
        false
    }

    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
    }

    pub fn dismiss_review(&self, path: &str) {
        self.store
            .update(|run| run.review.retain(|entry| entry.path != path));
    }

    /// The hosted service's loop (`ExecuteAsync`): one request at a time until `stopping`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut queue = self.receiver.lock().await;
        loop {
            let request = tokio::select! {
                request = queue.recv() => request,
                () = stopping.cancelled() => break,
            };
            let Some(request) = request else {
                break;
            };
            let result = self.run_request(&request, &stopping).await;
            let stop = match result {
                Ok(()) => false,
                Err(error) if error.is::<Cancelled>() && stopping.is_cancelled() => {
                    self.store
                        .update(|run| run.status = LyricsLibraryStatus::Interrupted);
                    true
                }
                Err(error) => {
                    error!("Finding lyrics for the library failed: {error:#}");
                    self.store.update(|run| {
                        run.status = LyricsLibraryStatus::Failed;
                        run.reason = Some(error.to_string());
                        run.finished_utc = Some(Utc::now());
                    });
                    false
                }
            };
            self.pending.store(false, Ordering::SeqCst);
            self.store.flush();
            if stop {
                break;
            }
        }
        Ok(())
    }

    /// One request (`RunAsync`): a walk here, the lyrics page's steps in `lyrics_library_steps`.
    /// An error is what the C# threw, which the loop records as a failed run.
    pub async fn run_request(
        &self,
        request: &LyricsLibraryRequest,
        stopping: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mode = if request.resume {
            self.store.read(|run| run.mode)
        } else {
            request.mode
        };
        if mode != LyricsLibraryMode::Walk {
            return self.run_step(request, mode, stopping).await;
        }
        self.cancel_requested.store(false, Ordering::SeqCst);
        let queue: Vec<String>;
        if request.resume && self.store.read(LyricsLibraryRun::can_resume) {
            queue = self.store.read(|run| run.queue.clone());
            self.store.update(|run| {
                run.status = LyricsLibraryStatus::Running;
                run.reason = None;
            });
            info!(
                "Finding lyrics for the library, resuming at {}/{}",
                self.store.read(|run| run.cursor),
                queue.len()
            );
        } else {
            let whole = self.settings.current().metadata.write_lyrics_beside_all_songs;
            queue = self.enumerate(whole).await;
            // A new run keeps what an earlier one left for review.
            let review = self.store.read(|run| run.review.clone());
            self.store.replace(LyricsLibraryRun {
                run_id: new_run_id(),
                status: LyricsLibraryStatus::Running,
                mode: LyricsLibraryMode::Walk,
                scope: if whole { WHOLE_LIBRARY } else { OCTO_DOWNLOADS }.to_string(),
                upgrade: request.upgrade,
                started_utc: Some(Utc::now()),
                total: count(queue.len()),
                queue: queue.clone(),
                review,
                ..LyricsLibraryRun::default()
            });
            info!(
                "Finding lyrics for the library: {} song(s), {}",
                queue.len(),
                self.store.read(|run| run.scope.clone())
            );
        }

        let upgrade = self.store.read(|run| run.upgrade);
        let mut busy_in_a_row = 0;
        let start = self.store.read(|run| run.cursor).max(0) as usize;
        for (index, path) in queue.iter().enumerate().skip(start) {
            if stopping.is_cancelled() {
                self.store
                    .update(|run| run.status = LyricsLibraryStatus::Interrupted);
                return Ok(());
            }
            if self.cancel_requested.load(Ordering::SeqCst) {
                self.store
                    .update(|run| octo_core::lyrics::lyrics_library_steps::cancel(run, Utc::now()));
                return Ok(());
            }

            let (write, looked, failure) = self.process(path, upgrade, stopping).await?;
            let outcome = write.as_ref().map(|write| write.outcome);
            let busy = matches!(
                outcome,
                Some(LyricsWriteOutcome::GaveUp | LyricsWriteOutcome::Retrying)
            );
            busy_in_a_row = if busy { busy_in_a_row + 1 } else { 0 };
            if busy_in_a_row >= Self::BUSY_IN_A_ROW_LIMIT {
                // The songs that met the wall are not counted as done, so a resume asks again.
                let from = count(index) - Self::BUSY_IN_A_ROW_LIMIT + 1;
                self.store.update(|run| {
                    run.status = LyricsLibraryStatus::Interrupted;
                    run.cursor = from;
                    run.busy -= Self::BUSY_IN_A_ROW_LIMIT - 1;
                    run.processed -= Self::BUSY_IN_A_ROW_LIMIT - 1;
                    run.reason = Some(SERVICES_STOPPED_ANSWERING.to_string());
                });
                warn!(
                    "Finding lyrics for the library paused at {from}/{}: the services are not answering",
                    queue.len()
                );
                return Ok(());
            }

            self.store.update(|run| {
                run.cursor = count(index) + 1;
                run.processed += 1;
                run.last_path = Some(path.clone());
                if let Some(failure) = &failure {
                    run.failed += 1;
                    run.errors.push(format!("{path}: {failure}"));
                    return;
                }
                let timing = write
                    .as_ref()
                    .and_then(|write| write.result.as_ref())
                    .map(|result| result.timing());
                match outcome {
                    Some(LyricsWriteOutcome::Written) => {
                        run.written += 1;
                        if timing == Some(LyricsTiming::Word) {
                            run.word_timed += 1;
                        }
                    }
                    Some(LyricsWriteOutcome::Upgraded) => {
                        run.upgraded += 1;
                        if timing == Some(LyricsTiming::Word) {
                            run.word_timed += 1;
                        }
                    }
                    Some(LyricsWriteOutcome::AlreadyThere) => run.already_had += 1,
                    Some(LyricsWriteOutcome::NotFound) => run.not_found += 1,
                    Some(LyricsWriteOutcome::Instrumental) => run.instrumental += 1,
                    Some(LyricsWriteOutcome::GaveUp | LyricsWriteOutcome::Retrying) => run.busy += 1,
                    _ => run.skipped += 1,
                }
                let written = matches!(
                    outcome,
                    Some(LyricsWriteOutcome::Written | LyricsWriteOutcome::Upgraded)
                );
                let result = write.as_ref().and_then(|write| write.result.as_ref());
                if let (true, Some(result), Some(tags)) = (written, result, looked.as_ref())
                    && let Some(doubt) = &result.doubt
                {
                    run.review.retain(|entry| entry.path != *path);
                    run.review.push(LyricsReviewEntry {
                        path: path.clone(),
                        artist: tags.artist.clone(),
                        title: tags.title.clone(),
                        album: tags.album.clone(),
                        duration_seconds: tags.duration_seconds,
                        source: result.source.clone(),
                        kind: LyricsChoiceService::kind_of(result).to_string(),
                        candidate_id: result.candidate_id.clone(),
                        reason: doubt.clone(),
                        at_utc: Utc::now(),
                    });
                }
            });

            if looked.is_some() && outcome != Some(LyricsWriteOutcome::AlreadyThere) {
                self.pause(stopping).await?;
            }
        }

        self.store.update(|run| {
            run.status = LyricsLibraryStatus::Completed;
            run.finished_utc = Some(Utc::now());
        });
        let done = self.store.current();
        info!(
            "Finding lyrics for the library finished: {} written ({} word-timed), {} upgraded, {} already had lyrics, {} not found, {} busy, of {}",
            done.written,
            done.word_timed,
            done.upgraded,
            done.already_had,
            done.not_found,
            done.busy,
            done.total
        );
        Ok(())
    }

    /// The pause after a looked-up song (`Task.Delay(Gap, stoppingToken)`), which a shutdown
    /// cuts short with [`Cancelled`].
    pub(super) async fn pause(&self, stopping: &CancellationToken) -> anyhow::Result<()> {
        if self.gap.is_zero() {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(self.gap) => Ok(()),
            () = stopping.cancelled() => Err(Cancelled.into()),
        }
    }

    /// One song: its tags, then the writer. Looked is None when the song was never looked up (no
    /// tags to look it up by). An I/O error is the song's; any other error ends the run.
    async fn process(
        &self,
        path: &str,
        upgrade: bool,
        stopping: &CancellationToken,
    ) -> anyhow::Result<Processed> {
        let Some(job) = self.read_tags(path) else {
            return Ok((
                Some(LyricsWrite {
                    outcome: LyricsWriteOutcome::Gone,
                    result: None,
                }),
                None,
                None,
            ));
        };
        let attempt = job.clone().with_attempt(LyricsSidecarWriter::MAX_ATTEMPTS);
        // A shutdown during the lookup is no error: the lyrics service answers "not now" (as the
        // C# one did), the song is counted as busy, and the pause or the next song's check ends
        // the run Interrupted.
        match self.writer.write(&attempt, upgrade, stopping).await {
            Ok(write) => Ok((Some(write), Some(job), None)),
            Err(error) if error.chain().any(|cause| cause.is::<io::Error>()) => {
                Ok((None, None, Some(error.to_string())))
            }
            Err(error) => Err(error),
        }
    }

    /// What a song is looked up by, from its own tags. None without an artist and a title, since
    /// a lookup by file name is a guess.
    pub fn read_tags(&self, path: &str) -> Option<LyricsJob> {
        let tags = self.writer.tags().read_song(Path::new(path)).ok().flatten()?;
        let artist = tags.first_performer.or(tags.first_album_artist)?;
        let title = tags.title?;
        if is_null_or_white_space(Some(&artist)) || is_null_or_white_space(Some(&title)) {
            return None;
        }
        let artist = artist.trim();
        let album = tags
            .album
            .filter(|album| !is_null_or_white_space(Some(album)))
            .map(|album| album.trim().to_string());
        Some(LyricsJob::new(
            path,
            artist,
            LyricsText::query_title(title.trim(), artist),
            album,
            tags.duration_seconds.filter(|seconds| *seconds > 0),
        ))
    }

    /// The songs a run walks, in ordinal order: Octo's downloads (from `.mappings.json`, those
    /// still on disk, once each ignoring case), or every audio file under the music folder.
    pub(super) async fn enumerate(&self, whole_library: bool) -> Vec<String> {
        if !whole_library {
            let mut seen = HashSet::new();
            let mut paths: Vec<String> = self
                .library
                .get_mappings()
                .await
                .into_iter()
                .map(|mapping| mapping.local_path)
                .filter(|path| !path.is_empty() && Path::new(path).is_file())
                .filter(|path| seen.insert(ordinal_ignore_case_key(path)))
                .collect();
            paths.sort_by(|a, b| compare_ordinal(a, b));
            return paths;
        }

        let root = (self.music_root)();
        if root.is_empty() || !Path::new(&root).is_dir() {
            warn!("Finding lyrics for the library found no music folder at {root}");
            return Vec::new();
        }
        let listed = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || audio_files(Path::new(&root))).await
        };
        match listed {
            Ok(Ok(mut paths)) => {
                paths.sort_by(|a, b| compare_ordinal(a, b));
                paths
            }
            Ok(Err(error)) => {
                error!("Finding lyrics for the library could not list {root}: {error}");
                Vec::new()
            }
            Err(error) => {
                error!("Finding lyrics for the library could not list {root}: {error}");
                Vec::new()
            }
        }
    }
}

/// `Guid.NewGuid().ToString("N")[..12]`.
pub(super) fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// A list's length or index as the run's `int` counters hold it.
pub(super) fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// `StringComparer.Ordinal`: UTF-16 code unit order, which differs from Rust's (code point)
/// order above U+FFFF.
fn compare_ordinal(a: &str, b: &str) -> CmpOrdering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)`, kept to the audio
/// extensions. Symbolic links to folders are followed, and any folder that cannot be read fails
/// the whole listing (`IgnoreInaccessible` is off for that overload).
fn audio_files(root: &Path) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder)? {
            let entry = entry?;
            let path = entry.path();
            let is_dir = match entry.file_type()? {
                kind if kind.is_symlink() => std::fs::metadata(&path).is_ok_and(|target| target.is_dir()),
                kind => kind.is_dir(),
            };
            if is_dir {
                folders.push(path);
                continue;
            }
            let name = entry.file_name();
            let extension = extension_of(&name.to_string_lossy());
            if LyricsLibraryWorker::AUDIO_EXTENSIONS
                .iter()
                .any(|audio| eq_ignore_case(audio, &extension))
            {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    Ok(found)
}

/// `Path.GetExtension` of a file name: from its last dot, or nothing when there is no dot or the
/// name ends in one. (Unlike Rust's, ".mp3" has the extension ".mp3".)
fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "lyrics_library_job_tests.rs"]
mod tests;
