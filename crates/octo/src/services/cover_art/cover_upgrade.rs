//! Port of `Services/CoverArt/CoverUpgrade.cs`: "Upgrade cover art". The run and its store
//! (`cover-upgrade.json`, and the found covers' thumbnails in `cover-upgrade-found/`), the undo
//! journal (`cover-upgrade-journal.jsonl` and the old pictures in `cover-backups/`), and the
//! worker, a singleton that is also a hosted queue worker.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{eq_ignore_case, escape_data_string, is_blank, is_null_or_white_space};
use octo_core::settings::{MetadataSettings, SettingsStore};
use octo_media::cover::{cover_files, cover_image};
use octo_media::tags::{FRONT_COVER, TagFile, TagPicture};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::album_cover_finder::{AlbumCoverQuery, IAlbumCoverFinder};
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::parse_json;
use crate::services::local::ILocalLibraryService;
use crate::services::state_file::{self, null_as_default};
use crate::services::subsonic::NavidromeIdentityService;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum CoverUpgradeScope {
    /// Only the files Octo downloaded.
    #[default]
    OctoDownloads = 0,
    /// Every song under the music folder.
    WholeLibrary = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum CoverUpgradeStatus {
    #[default]
    Idle = 0,
    Running = 1,
    Completed = 2,
    Cancelled = 3,
    Interrupted = 4,
    Failed = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum CoverUpgradeMode {
    /// Reads the songs only, and lists every album whose cover is smaller than asked. No
    /// lookups and no writes, so it is quick even over a whole library.
    #[default]
    Scan = 0,
    /// Looks the albums up and lists what a larger cover would replace. Writes nothing.
    Preview = 1,
    /// Looks up and writes.
    Apply = 2,
}

/// One run. `albums` is the albums picked from the last run's list, by id; None means every
/// album in scope whose cover is smaller than `smaller_than`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverUpgradeRequest {
    pub scope: CoverUpgradeScope,
    pub mode: CoverUpgradeMode,
    pub folder_covers: bool,
    pub smaller_than: i32,
    pub albums: Option<Vec<String>>,
    pub undo: bool,
}

impl CoverUpgradeRequest {
    /// `new CoverUpgradeRequest(scope, mode, folderCovers)`, with the C# defaults for the rest.
    pub fn new(scope: CoverUpgradeScope, mode: CoverUpgradeMode, folder_covers: bool) -> Self {
        Self {
            scope,
            mode,
            folder_covers,
            smaller_than: CoverUpgradeWorker::DEFAULT_SMALLER_THAN,
            albums: None,
            undo: false,
        }
    }

    /// The same request limited to these albums.
    pub fn with_albums(mut self, albums: Vec<String>) -> Self {
        self.albums = Some(albums);
        self
    }

    /// The undo request (`Undo: true`).
    pub fn undo() -> Self {
        Self {
            undo: true,
            ..Self::new(CoverUpgradeScope::WholeLibrary, CoverUpgradeMode::Apply, true)
        }
    }
}

/// One album on the list. `result` says what became of it: `soft` (a scan found its cover
/// small), `found` (a preview found a larger one), `upgraded` (written), or `none` (looked up,
/// nothing clearly larger).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CoverUpgradeChange {
    #[serde(deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub folder: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    pub album: Option<String>,
    pub from_side: i32,
    pub to_side: i32,
    pub source: Option<String>,
    pub files: i32,
    pub folder_cover: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub result: String,
    pub first_file: Option<String>,
    pub paths: Option<Vec<String>>,
    pub navidrome_album_id: Option<String>,
    pub barcode: Option<String>,
    pub looks_same: Option<bool>,
}

/// One piece of work: a folder, or one Navidrome album when Navidrome could say which songs
/// make each album. `files` is None for "every song in the folder".
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CoverUpgradeItem {
    #[serde(deserialize_with = "null_as_default")]
    pub folder: String,
    pub files: Option<Vec<String>>,
    pub navidrome_album_id: Option<String>,
}

impl CoverUpgradeItem {
    pub fn new(
        folder: impl Into<String>,
        files: Option<Vec<String>>,
        navidrome_album_id: Option<String>,
    ) -> Self {
        Self {
            folder: folder.into(),
            files,
            navidrome_album_id,
        }
    }
}

/// The run as `cover-upgrade.json` holds it. `DryRun` and `CanResume` were `[JsonIgnore]`, so
/// they are methods here and never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CoverUpgradeRun {
    #[serde(deserialize_with = "null_as_default")]
    pub run_id: String,
    pub status: CoverUpgradeStatus,
    pub scope: CoverUpgradeScope,
    pub mode: CoverUpgradeMode,
    pub folder_covers: bool,
    pub smaller_than: i32,
    /// The album ids this run was limited to, or None for every album in scope.
    pub selected: Option<Vec<String>>,
    pub full_size: bool,
    pub undo: bool,
    #[serde(with = "octo_core::json::datetime::utc_option")]
    pub started_utc: Option<DateTime<Utc>>,
    #[serde(with = "octo_core::json::datetime::utc_option")]
    pub finished_utc: Option<DateTime<Utc>>,
    /// Folders in the queue.
    pub total: i32,
    pub processed: i32,
    /// Songs in the queue and songs read so far: the progress a person sees, because a flat
    /// library is one folder of thousands of songs.
    pub songs_total: i32,
    pub songs_read: i32,
    /// Albums to go through and gone through, when they are known up front: every album of a
    /// scan Navidrome grouped, or the picked ones. Lookups take seconds each, so this is the
    /// progress that moves during them.
    pub albums_total: i32,
    pub albums_done: i32,
    /// Albums a scan found with a cover smaller than asked.
    pub soft: i32,
    /// Albums whose cover got (or would get) larger.
    pub upgraded: i32,
    /// Albums already as sharp as anything found, or with nothing found.
    pub kept: i32,
    /// Songs rewritten (or that would be).
    pub files: i32,
    pub failed: i32,
    pub cursor: i32,
    pub last_folder: Option<String>,
    pub reason: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub errors: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub preview: Vec<CoverUpgradeChange>,
    #[serde(deserialize_with = "null_as_default")]
    pub queue: Vec<CoverUpgradeItem>,
}

impl Default for CoverUpgradeRun {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            status: CoverUpgradeStatus::Idle,
            scope: CoverUpgradeScope::OctoDownloads,
            mode: CoverUpgradeMode::Scan,
            folder_covers: true,
            smaller_than: CoverUpgradeWorker::DEFAULT_SMALLER_THAN,
            selected: None,
            full_size: false,
            undo: false,
            started_utc: None,
            finished_utc: None,
            total: 0,
            processed: 0,
            songs_total: 0,
            songs_read: 0,
            albums_total: 0,
            albums_done: 0,
            soft: 0,
            upgraded: 0,
            kept: 0,
            files: 0,
            failed: 0,
            cursor: 0,
            last_folder: None,
            reason: None,
            errors: Vec::new(),
            preview: Vec::new(),
            queue: Vec::new(),
        }
    }
}

impl CoverUpgradeRun {
    pub fn dry_run(&self) -> bool {
        self.mode != CoverUpgradeMode::Apply
    }

    pub fn can_resume(&self) -> bool {
        matches!(
            self.status,
            CoverUpgradeStatus::Cancelled | CoverUpgradeStatus::Interrupted
        ) && !self.undo
            && i64::from(self.cursor) < self.queue.len() as i64
    }
}

/// The run, kept in the config folder so it survives a restart; the genre backfill's idiom
/// (coalesced writes every ten seconds, through a temporary file).
///
/// The C# store ran its own 10 s timer and flushed once more on `Dispose`; here that is
/// [`CoverUpgradeStore::run_flusher`], registered as a worker by the app state.
pub struct CoverUpgradeStore {
    path: Option<PathBuf>,
    run: Mutex<CoverUpgradeRun>,
    dirty: AtomicBool,

    /// Small copies of the covers a preview found, by album id, so the dashboard can show the
    /// new cover before anything is written. On disk beside the run, or in memory when the
    /// store has no file (tests).
    thumbs: Option<PathBuf>,
    memory_thumbs: Mutex<HashMap<String, Vec<u8>>>,
}

impl CoverUpgradeStore {
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
    /// Enough for a scan of a large library to list every soft album, so any of them can be
    /// picked; about 250 bytes a row.
    pub const MAX_PREVIEW_ROWS: usize = 5000;
    const MAX_ERRORS: usize = 20;

    /// `path` None (or blank) keeps the run, and the found covers, in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.filter(|path| !is_blank(&path.to_string_lossy()));
        let thumbs = path
            .as_ref()
            .map(|path| parent_of(path).join("cover-upgrade-found"));
        let run = path.as_deref().and_then(load).unwrap_or_default();
        Self {
            path,
            run: Mutex::new(run),
            dirty: AtomicBool::new(false),
            thumbs,
            memory_thumbs: Mutex::new(HashMap::new()),
        }
    }

    /// A copy of the run as it stands.
    pub fn current(&self) -> CoverUpgradeRun {
        self.run.lock().clone()
    }

    /// Reads the run without copying it (its queue can be thousands of paths).
    pub fn read<R>(&self, read: impl FnOnce(&CoverUpgradeRun) -> R) -> R {
        read(&self.run.lock())
    }

    pub fn update(&self, change: impl FnOnce(&mut CoverUpgradeRun)) {
        {
            let mut run = self.run.lock();
            change(&mut run);
            if run.errors.len() > Self::MAX_ERRORS {
                let excess = run.errors.len() - Self::MAX_ERRORS;
                run.errors.drain(..excess);
            }
            run.preview.truncate(Self::MAX_PREVIEW_ROWS);
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    pub fn save_found_thumb(&self, id: &str, bytes: Vec<u8>) {
        let Some(thumbs) = &self.thumbs else {
            self.memory_thumbs.lock().insert(id.to_string(), bytes);
            return;
        };
        let written = std::fs::create_dir_all(thumbs)
            .and_then(|()| std::fs::write(thumbs.join(format!("{id}.jpg")), bytes));
        if let Err(failure) = written {
            debug!("cover upgrade could not keep a found cover: {failure}");
        }
    }

    pub fn found_thumb(&self, id: &str) -> Option<Vec<u8>> {
        let Some(thumbs) = &self.thumbs else {
            return self.memory_thumbs.lock().get(id).cloned();
        };
        let path = thumbs.join(format!("{id}.jpg"));
        if path.is_file() {
            std::fs::read(path).ok()
        } else {
            None
        }
    }

    pub fn clear_found_thumbs(&self) {
        self.memory_thumbs.lock().clear();
        if let Some(thumbs) = self.thumbs.as_ref().filter(|thumbs| thumbs.is_dir())
            && let Err(failure) = std::fs::remove_dir_all(thumbs)
        {
            debug!("cover upgrade could not clear found covers: {failure}");
        }
    }

    pub fn replace(&self, run: CoverUpgradeRun) {
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
        if let Err(failure) = state_file::write_atomic(path, json.as_bytes()) {
            self.dirty.store(true, Ordering::SeqCst);
            warn!("cover upgrade state could not be written: {failure}");
        }
    }

    /// The C# flush timer and `Dispose`: flushes every [`Self::FLUSH_INTERVAL`] until
    /// cancelled, then once more.
    pub async fn run_flusher(&self, stopping: CancellationToken) {
        if self.path.is_none() {
            return;
        }
        let mut ticks = tokio::time::interval_at(
            tokio::time::Instant::now() + Self::FLUSH_INTERVAL,
            Self::FLUSH_INTERVAL,
        );
        loop {
            tokio::select! {
                _ = stopping.cancelled() => break,
                _ = ticks.tick() => self.flush(),
            }
        }
        self.flush();
    }
}

fn load(path: &Path) -> Option<CoverUpgradeRun> {
    if !path.exists() {
        return None;
    }
    let read = state_file::read_all_text(path)
        .map_err(|failure| failure.to_string())
        .and_then(|text| {
            serde_json::from_str::<Option<CoverUpgradeRun>>(&text).map_err(|failure| failure.to_string())
        });
    let mut run = match read {
        Ok(run) => run?,
        Err(failure) => {
            warn!("cover upgrade state could not be read: {failure}");
            return None;
        }
    };
    // Never resumed by itself: the restart may have been how it was stopped.
    if run.status == CoverUpgradeStatus::Running {
        run.status = CoverUpgradeStatus::Interrupted;
        run.reason = Some("Octo restarted while this run was going.".to_string());
    }
    Some(run)
}

/// One line of `cover-upgrade-journal.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverUpgradeJournalEntry {
    #[serde(rename = "p", default, deserialize_with = "null_as_default")]
    pub path: String,
    #[serde(rename = "k", default, deserialize_with = "null_as_default")]
    pub kind: String,
    #[serde(rename = "h", default)]
    pub hash: Option<String>,
    #[serde(rename = "r", default, deserialize_with = "null_as_default")]
    pub run_id: String,
}

impl CoverUpgradeJournalEntry {
    pub fn new(path: impl Into<String>, kind: &str, hash: Option<String>, run_id: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: kind.to_string(),
            hash,
            run_id: run_id.into(),
        }
    }
}

/// What the last real run replaced, so Undo can put it back: one line per file, and the old
/// pictures themselves in a folder beside it, stored once per distinct picture (an album's
/// tracks share one), named by their hash.
pub struct CoverUpgradeJournal {
    path: Option<PathBuf>,
    backups: Option<PathBuf>,
    lock: Mutex<()>,
}

impl CoverUpgradeJournal {
    pub const EMBEDDED: &'static str = "embedded";
    pub const FOLDER_FILE: &'static str = "file";

    /// `path` None (or blank) records nothing.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.filter(|path| !is_blank(&path.to_string_lossy()));
        let backups = path.as_ref().map(|path| parent_of(path).join("cover-backups"));
        Self {
            path,
            backups,
            lock: Mutex::new(()),
        }
    }

    pub fn has_entries(&self) -> bool {
        self.path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .is_some_and(|meta| meta.is_file() && meta.len() > 0)
    }

    /// Keeps the picture and records the file. False when the picture could not be kept, and
    /// then the file must not be changed: its undo would be gone.
    pub fn record(&self, path: &str, kind: &str, before: Option<&[u8]>, run_id: &str) -> bool {
        let Some(journal) = &self.path else {
            return true;
        };
        let attempt = || -> io::Result<()> {
            let mut hash = None;
            if let Some(before) = before.filter(|before| !before.is_empty()) {
                let name = hex::encode(Sha256::digest(before))[..32].to_string();
                let backups = self
                    .backups
                    .as_ref()
                    .expect("a journal with a file has a backups folder");
                let backup = backups.join(&name);
                std::fs::create_dir_all(backups)?;
                if !backup.exists() {
                    state_file::write_atomic(&backup, before)?;
                }
                hash = Some(name);
            }
            let line =
                octo_core::json::to_string(&CoverUpgradeJournalEntry::new(path, kind, hash, run_id)) + "\n";
            let _guard = self.lock.lock();
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(journal)?
                .write_all(line.as_bytes())
        };
        match attempt() {
            Ok(()) => true,
            Err(failure) => {
                warn!("cover upgrade could not keep the old cover of {path}: {failure}");
                false
            }
        }
    }

    /// Newest first. A run killed mid-line costs that one file's undo.
    pub fn read_all(&self) -> Vec<CoverUpgradeJournalEntry> {
        let Some(path) = self.path.as_ref().filter(|path| path.exists()) else {
            return Vec::new();
        };
        let text = {
            let _guard = self.lock.lock();
            state_file::read_all_text(path)
        };
        let Ok(text) = text else {
            return Vec::new();
        };
        let mut entries: Vec<CoverUpgradeJournalEntry> = state_file::lines(&text)
            .into_iter()
            .filter(|line| !is_blank(line))
            .filter_map(|line| {
                serde_json::from_str::<Option<CoverUpgradeJournalEntry>>(line)
                    .ok()
                    .flatten()
            })
            .filter(|entry| !entry.path.is_empty())
            .collect();
        entries.reverse();
        entries
    }

    pub fn backup(&self, hash: Option<&str>) -> Option<Vec<u8>> {
        let path = self.backups.as_ref()?.join(hash?);
        if path.is_file() {
            std::fs::read(path).ok()
        } else {
            None
        }
    }

    /// Keeps only these entries (oldest first), and the backups they still need. An error is
    /// what the C# threw.
    pub fn rewrite(&self, oldest_first: &[CoverUpgradeJournalEntry]) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let _guard = self.lock.lock();
        if oldest_first.is_empty() {
            match std::fs::remove_file(path) {
                Err(failure) if failure.kind() != io::ErrorKind::NotFound => return Err(failure),
                _ => {}
            }
        } else {
            // File.WriteAllLines: every line ends with a newline.
            let text: String = oldest_first
                .iter()
                .map(|entry| octo_core::json::to_string(entry) + "\n")
                .collect();
            state_file::write_atomic(path, text.as_bytes())?;
        }
        let Some(backups) = self.backups.as_ref().filter(|backups| backups.is_dir()) else {
            return Ok(());
        };
        let needed: HashSet<&str> = oldest_first
            .iter()
            .filter_map(|entry| entry.hash.as_deref())
            .collect();
        for file in std::fs::read_dir(backups)? {
            let file = file?;
            if file.file_type()?.is_dir() {
                continue;
            }
            if !needed.contains(file.file_name().to_string_lossy().as_ref()) {
                std::fs::remove_file(file.path())?;
            }
        }
        Ok(())
    }
}

/// What the worker resolved from a DI scope in C#. None stands for a container without it (the
/// C# tests' empty service provider).
#[derive(Clone, Default)]
pub struct CoverUpgradeServices {
    pub library: Option<Arc<dyn ILocalLibraryService>>,
    pub identity: Option<Arc<NavidromeIdentityService>>,
    /// `IHttpClientFactory.CreateClient()`.
    pub http: Option<reqwest::Client>,
}

/// One song as a run reads it.
#[derive(Debug, Clone)]
struct SongFile {
    path: String,
    artist: String,
    album: Option<String>,
    title: Option<String>,
    release_id: Option<String>,
    release_group_id: Option<String>,
    side: i32,
    barcode: Option<String>,
    looks: Option<u64>,
}

/// Why a folder's work stopped short of an answer.
enum FolderError {
    /// What the C# per-item catch recorded as a failure.
    Failed(String),
    /// Shutdown cut a lookup short (the `OperationCanceledException` the C# let through).
    Stopped,
}

/// The shutdown cut a run short mid-lookup: the loop ends with the run left as it was, as the
/// C# `catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested)` did.
#[derive(Debug)]
struct StoppedByShutdown;

impl std::fmt::Display for StoppedByShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The operation was canceled.")
    }
}

impl std::error::Error for StoppedByShutdown {}

/// What the C# `GetRequiredService<ILocalLibraryService>()` threw when the container had none.
const NO_LIBRARY_SERVICE: &str =
    "No service for type 'Octo.Services.Local.ILocalLibraryService' has been registered.";

/// Goes through a library's songs and gives each album the largest cover any source has for
/// it. Brandon's ask, 2026-10-01: "go through your artwork and grab the highest quality for
/// each one to embed all of your songs".
///
/// Three steps, Brandon's shape for it (2026-10-01): a scan reads the songs only and lists every
/// album whose cover is smaller than asked; he picks all of them or some; a preview of the
/// picked ones looks them up, and an upgrade writes. One folder at a time, its songs grouped by
/// album. An album is changed only when the cover found is clearly larger than the one its
/// songs carry, and only the front cover is replaced; other pictures stay. A real run keeps
/// every replaced picture first, so Undo can put it all back. cover.jpg and folder.jpg beside
/// an album are what Navidrome shows before the art inside the files, so they are upgraded too
/// when asked (JPEG only; a PNG or WebP one is left and reported).
pub struct CoverUpgradeWorker {
    store: Arc<CoverUpgradeStore>,
    journal: Arc<CoverUpgradeJournal>,
    finder: Arc<dyn IAlbumCoverFinder>,
    settings: Arc<SettingsStore>,
    services: CoverUpgradeServices,
    cancel: AtomicBool,
    pending: AtomicBool,
    sender: mpsc::Sender<CoverUpgradeRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<CoverUpgradeRequest>>,
}

impl CoverUpgradeWorker {
    /// A found cover must be this much larger to be worth a rewrite.
    pub const MINIMUM_GAIN: f64 = 1.2;

    /// What counts as a soft cover unless the dashboard says otherwise: under the catalog's
    /// own 1000 px.
    pub const DEFAULT_SMALLER_THAN: i32 = 1000;

    /// Albums a scan reads at once.
    pub const SCAN_PARALLELISM: usize = 6;

    /// Albums looked up at once. Apple's searches wait their turn inside the iTunes lookup
    /// (about 20 a minute), so this only overlaps the downloads and the other sources.
    pub const LOOKUP_PARALLELISM: usize = 4;

    /// The side of a dashboard tile's picture: sharp at twice the tile's size.
    pub const THUMB_SIDE: u32 = 320;

    pub const AUDIO_EXTENSIONS: [&'static str; 14] = [
        ".mp3", ".flac", ".m4a", ".mp4", ".aac", ".ogg", ".oga", ".opus", ".wma", ".aif", ".aiff", ".dsf",
        ".wv", ".ape",
    ];

    pub fn new(
        store: Arc<CoverUpgradeStore>,
        journal: Arc<CoverUpgradeJournal>,
        finder: Arc<dyn IAlbumCoverFinder>,
        settings: Arc<SettingsStore>,
        services: CoverUpgradeServices,
    ) -> Self {
        // Capacity 1, and a request past it is dropped (`DropWrite`): `pending` keeps a second
        // one from ever being written while one waits.
        let (sender, receiver) = mpsc::channel(1);
        Self {
            store,
            journal,
            finder,
            settings,
            services,
            cancel: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
        }
    }

    pub fn current(&self) -> CoverUpgradeRun {
        self.store.current()
    }

    pub fn store(&self) -> &Arc<CoverUpgradeStore> {
        &self.store
    }

    pub fn can_undo(&self) -> bool {
        self.journal.has_entries()
    }

    pub fn is_busy(&self) -> bool {
        self.store.read(|run| run.status == CoverUpgradeStatus::Running)
            || self.pending.load(Ordering::SeqCst)
    }

    /// False when a run is going or waiting to start (the controller's 409).
    pub fn try_enqueue(&self, request: CoverUpgradeRequest) -> bool {
        if self.store.read(|run| run.status == CoverUpgradeStatus::Running)
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
        self.cancel.store(true, Ordering::SeqCst);
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
            self.cancel.store(false, Ordering::SeqCst);
            let result = if request.undo {
                self.undo(&stopping).await
            } else {
                self.run_request(&request, &stopping).await
            };
            let stop = match result {
                Ok(()) => false,
                Err(failure) if failure.is::<StoppedByShutdown>() && stopping.is_cancelled() => true,
                Err(failure) => {
                    error!("Cover upgrade failed: {failure:#}");
                    self.store.update(|run| {
                        run.status = CoverUpgradeStatus::Failed;
                        run.reason = Some(failure.to_string());
                        run.finished_utc = Some(Utc::now());
                    });
                    false
                }
            };
            self.pending.store(false, Ordering::SeqCst);
            if stop {
                break;
            }
        }
        Ok(())
    }

    /// One scan, preview or upgrade (`RunAsync`).
    async fn run_request(
        &self,
        request: &CoverUpgradeRequest,
        stopping: &CancellationToken,
    ) -> anyhow::Result<()> {
        let previous = self.store.current();
        let selected: Option<Vec<String>> = request.albums.as_ref().map(|albums| {
            let mut seen = HashSet::new();
            albums
                .iter()
                .filter(|id| seen.insert(id.as_str()))
                .cloned()
                .collect()
        });
        let resuming = previous.can_resume()
            && previous.scope == request.scope
            && previous.mode == request.mode
            && previous.folder_covers == request.folder_covers
            && previous.smaller_than == request.smaller_than
            && previous.selected.as_deref().unwrap_or_default() == selected.as_deref().unwrap_or_default()
            && previous.selected.is_none() == selected.is_none();
        if resuming {
            self.store.update(|run| {
                run.status = CoverUpgradeStatus::Running;
                run.reason = None;
            });
        } else {
            // Running from the first moment, with nothing listed yet: listing the songs and
            // asking Navidrome about them takes seconds, and a dashboard that looked in that
            // time saw the last run and never noticed this one start.
            self.store.replace(CoverUpgradeRun {
                run_id: new_run_id(),
                status: CoverUpgradeStatus::Running,
                scope: request.scope,
                mode: request.mode,
                folder_covers: request.folder_covers,
                smaller_than: request.smaller_than,
                selected: selected.clone(),
                full_size: self.settings.current().metadata.embed_full_size_covers,
                started_utc: Some(Utc::now()),
                reason: Some("Listing your songs.".to_string()),
                ..CoverUpgradeRun::default()
            });
            let queue = match &selected {
                None => self.enumerate(request.scope).await?,
                Some(selected) => Self::queue_of(&previous, selected),
            };
            if request.mode != CoverUpgradeMode::Apply {
                self.store.clear_found_thumbs();
            }
            self.store.update(|run| {
                run.reason = None;
                run.total = count(queue.len());
                run.songs_total = count(
                    queue
                        .iter()
                        .map(|item| item.files.as_ref().map_or(0, Vec::len))
                        .sum(),
                );
                run.albums_total = count(match &selected {
                    Some(selected) => selected.len(),
                    None => queue
                        .iter()
                        .filter(|item| item.navidrome_album_id.is_some())
                        .count(),
                });
                run.queue = queue;
            });
        }
        let current = self.store.current();
        info!(
            "Cover upgrade {:?}: {} folder(s) from {}, scope {:?}, {}",
            current.mode,
            current.total,
            current.cursor,
            current.scope,
            match &current.selected {
                None => "every album".to_string(),
                Some(selected) => format!("{} picked album(s)", selected.len()),
            }
        );

        // Picked albums are matched in bulk (barcodes, then Apple 20 to 40 at a time) WHILE the
        // lookups run: each album waits only for its own batch, so the run takes about as long
        // as finding the barcodes, not that plus everything after it.
        let queries: Option<Vec<AlbumCoverQuery>> = match &current.selected {
            Some(picks) if current.mode != CoverUpgradeMode::Scan && !picks.is_empty() => {
                let picked: HashSet<&str> = picks.iter().map(String::as_str).collect();
                Some(
                    previous
                        .preview
                        .iter()
                        .filter(|row| {
                            picked.contains(row.id.as_str()) && !is_null_or_white_space(row.album.as_deref())
                        })
                        .map(|row| AlbumCoverQuery {
                            barcode: row.barcode.clone(),
                            ..AlbumCoverQuery::new(row.artist.clone(), row.album.clone(), None)
                        })
                        .collect(),
                )
            }
            _ => None,
        };
        let Some(queries) = queries else {
            return self.look_up_albums(&current, stopping).await;
        };

        let report = |text: String| self.store.update(|run| run.reason = Some(text));
        let priming = async {
            if let Err(failure) = self.finder.prime(&queries, Some(&report)).await {
                info!("Cover upgrade could not match albums in bulk, so each is searched: {failure}");
            }
            self.store.update(|run| run.reason = None);
        };
        let lookups = self.look_up_albums(&current, stopping);
        tokio::pin!(priming, lookups);
        let mut primed = false;
        let result = loop {
            tokio::select! {
                result = &mut lookups => break result,
                () = &mut priming, if !primed => primed = true,
            }
        };
        if !primed {
            // Stopped part way: its `finally` still clears what it was saying.
            self.store.update(|run| run.reason = None);
        }
        result
    }

    async fn look_up_albums(
        &self,
        current: &CoverUpgradeRun,
        stopping: &CancellationToken,
    ) -> anyhow::Result<()> {
        // Over a network mount waiting is most of each read, and most of each lookup is waiting
        // on a server, so several albums are worked on at once.
        let parallel = if current.mode == CoverUpgradeMode::Scan {
            Self::SCAN_PARALLELISM
        } else {
            Self::LOOKUP_PARALLELISM
        };
        let mut index = current.cursor.max(0) as usize;
        while index < current.queue.len() {
            if self.stopped(stopping) {
                return Ok(());
            }

            let batch = &current.queue[index..(index + parallel).min(current.queue.len())];
            let looked = futures::future::join_all(batch.iter().map(|item| async move {
                match self.process_folder(item, current, stopping).await {
                    Ok(looked) => Ok(looked),
                    Err(FolderError::Failed(message)) if !stopping.is_cancelled() => {
                        self.store.update(|run| {
                            run.failed += 1;
                            run.errors.push(format!("{}: {message}", item.folder));
                        });
                        Ok(false)
                    }
                    Err(_) => Err(StoppedByShutdown),
                }
            }))
            .await;
            if looked.iter().any(Result::is_err) {
                return Err(StoppedByShutdown.into());
            }
            // Stopped part way through: this batch is not done, so it runs again on a resume.
            // It used to count as done, and a resume skipped the rest of a half-read folder
            // (which in a flat library was the whole library).
            if stopping.is_cancelled() || self.cancel.load(Ordering::SeqCst) {
                index += parallel;
                continue;
            }
            let reached = index + batch.len();
            self.store.update(|run| {
                run.cursor = count(reached);
                run.processed += count(batch.len());
                run.last_folder = batch.last().map(|item| item.folder.clone());
            });
            index += parallel;
        }

        // A stop during the last batch ends the loop too; that batch did not finish.
        if self.stopped(stopping) {
            return Ok(());
        }
        self.store.update(|run| {
            run.status = CoverUpgradeStatus::Completed;
            run.finished_utc = Some(Utc::now());
        });
        let done = self.store.current();
        info!(
            "Cover upgrade {:?} finished: {} soft, {} larger, {} song(s), {} kept, {} failed",
            done.mode, done.soft, done.upgraded, done.files, done.kept, done.failed
        );
        if !done.dry_run() && done.files > 0 {
            self.rescan().await;
        }
        Ok(())
    }

    /// Records a shutdown or a stop from the dashboard. True when the run must end.
    fn stopped(&self, stopping: &CancellationToken) -> bool {
        if stopping.is_cancelled() {
            self.store
                .update(|run| run.status = CoverUpgradeStatus::Interrupted);
            return true;
        }
        if !self.cancel.load(Ordering::SeqCst) {
            return false;
        }
        self.store.update(|run| {
            run.status = CoverUpgradeStatus::Cancelled;
            run.reason = Some("Stopped from the dashboard.".to_string());
            run.finished_utc = Some(Utc::now());
        });
        true
    }

    fn halted(&self, stopping: &CancellationToken) -> bool {
        stopping.is_cancelled() || self.cancel.load(Ordering::SeqCst)
    }

    /// The songs behind the picked albums, from the list they were picked from. Only those
    /// songs are read again: in a flat library every album shares one folder of thousands of
    /// songs, and reading the folder to find three albums took as long as the scan.
    fn queue_of(previous: &CoverUpgradeRun, ids: &[String]) -> Vec<CoverUpgradeItem> {
        let picked: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut files: HashMap<&str, Option<&Vec<String>>> = HashMap::new();
        for item in &previous.queue {
            files.entry(item.folder.as_str()).or_insert(item.files.as_ref());
        }
        let mut groups: Vec<(&str, Vec<&CoverUpgradeChange>)> = Vec::new();
        for row in previous
            .preview
            .iter()
            .filter(|row| picked.contains(row.id.as_str()))
        {
            match groups.iter_mut().find(|(folder, _)| *folder == row.folder) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((row.folder.as_str(), vec![row])),
            }
        }
        groups.sort_by(|(a, _), (b, _)| compare_ordinal(a, b));
        let has_paths = |row: &&CoverUpgradeChange| row.paths.as_ref().is_some_and(|paths| !paths.is_empty());
        groups
            .into_iter()
            .flat_map(|(folder, rows)| {
                if rows
                    .iter()
                    .all(|row| row.navidrome_album_id.is_some() && has_paths(row))
                {
                    // Navidrome albums stay one item each, so they keep their ids.
                    rows.iter()
                        .map(|row| {
                            CoverUpgradeItem::new(folder, row.paths.clone(), row.navidrome_album_id.clone())
                        })
                        .collect::<Vec<_>>()
                } else {
                    let files = if rows.iter().all(has_paths) {
                        let mut seen = HashSet::new();
                        Some(
                            rows.iter()
                                .flat_map(|row| row.paths.iter().flatten())
                                .filter(|path| seen.insert(path.as_str()))
                                .cloned()
                                .collect(),
                        )
                    } else {
                        files.get(folder).copied().flatten().cloned()
                    };
                    vec![CoverUpgradeItem::new(folder, files, None)]
                }
            })
            .collect()
    }

    /// A stable id for one album in one folder, so a pick survives from one run to the next.
    pub fn album_id(folder: &str, album_key: &str) -> String {
        hex::encode(Sha256::digest(format!("{folder}\u{0}{album_key}").as_bytes()))[..16].to_string()
    }

    /// A small copy of the cover a preview found for an album on the list.
    pub fn found_thumbnail(&self, id: &str) -> Option<Vec<u8>> {
        self.store.found_thumb(id)
    }

    /// A small copy of the cover an album on the list has now, for the dashboard. Reads files:
    /// call it from blocking code.
    pub fn thumbnail(&self, id: &str) -> Option<Vec<u8>> {
        let row = self
            .store
            .read(|run| run.preview.iter().find(|row| row.id == id).cloned())?;
        let path = row
            .first_file
            .as_deref()
            .filter(|path| Path::new(path).is_file())?;
        let file = TagFile::open(path).ok()?;
        let mut bytes = front_of(&file.pictures()).map(|picture| picture.data);
        if bytes.as_ref().is_none_or(Vec::is_empty)
            && let Ok(Some((folder_file, _))) = self.folder_cover(&row.folder, true)
        {
            bytes = std::fs::read(folder_file).ok();
        }
        bytes
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| cover_image::to_jpeg(&cover_image::fit_within(&bytes, Self::THUMB_SIDE)))
    }

    /// True when it asked the sources anything, so the run should pause after it.
    async fn process_folder(
        &self,
        item: &CoverUpgradeItem,
        run: &CoverUpgradeRun,
        stopping: &CancellationToken,
    ) -> Result<bool, FolderError> {
        if !Path::new(&item.folder).is_dir() {
            return Ok(false);
        }
        let paths: Vec<String> = match &item.files {
            Some(files) => files.clone(),
            None => audio_files_in(Path::new(&item.folder))
                .map_err(|failure| FolderError::Failed(failure.to_string()))?,
        };
        let mut songs: Vec<SongFile> = Vec::new();
        // A Navidrome album is known to be one album, so a scan or a lookup needs only one of
        // its songs read; a replace reads every song, since each one gets its own cover.
        let one_will_do = item.navidrome_album_id.is_some() && run.mode != CoverUpgradeMode::Apply;
        for path in &paths {
            if self.halted(stopping) {
                return Ok(false);
            }
            let song = if Path::new(path).is_file() {
                let path = path.clone();
                tokio::task::spawn_blocking(move || read_song(&path))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let read_one = song.is_some();
            if let Some(song) = song {
                songs.push(song);
            }
            self.store.update(|r| r.songs_read += 1);
            if one_will_do && read_one {
                let at = paths.iter().position(|p| p == path).unwrap_or_default();
                let left = paths.len() - at - 1;
                if left > 0 {
                    self.store.update(|r| r.songs_read += count(left));
                }
                break;
            }
        }

        let mut looked = false;
        let picked: Option<HashSet<&str>> = run
            .selected
            .as_ref()
            .map(|selected| selected.iter().map(String::as_str).collect());
        // An album without a name is matched song by song, as a single.
        let mut albums: Vec<(String, Vec<&SongFile>)> = Vec::new();
        for song in &songs {
            let key = match (&item.navidrome_album_id, song.album.as_deref()) {
                (Some(album), _) => format!("nd:{album}"),
                (None, album) if is_null_or_white_space(album) => format!("\u{1}{}", song.path),
                (None, album) => format!(
                    "{}|{}",
                    SongIdentity::key(&song.artist),
                    SongIdentity::key(album.unwrap_or_default())
                ),
            };
            match albums.iter_mut().find(|(known, _)| *known == key) {
                Some((_, members)) => members.push(song),
                None => albums.push((key, vec![song])),
            }
        }
        for (key, album) in &albums {
            if self.halted(stopping) {
                break;
            }
            let id = Self::album_id(&item.folder, key);
            self.store.update(|r| {
                if r.albums_total > 0 {
                    r.albums_done += 1;
                }
            });
            // Every path of the album, read or not, so a later run and a replace see them all.
            let album_paths: Vec<String> = if item.navidrome_album_id.is_some() {
                paths.clone()
            } else {
                album.iter().map(|song| song.path.clone()).collect()
            };
            if picked
                .as_ref()
                .is_some_and(|picked| !picked.contains(id.as_str()))
            {
                continue;
            }
            let first = album[0];
            let have = album.iter().map(|song| song.side).min().unwrap_or_default();
            let holds_only = if item.navidrome_album_id.is_some() {
                folder_holds_only(&item.folder, &album_paths)
            } else {
                album.len() == songs.len()
            };
            let folder_cover = if run.folder_covers && holds_only {
                self.folder_cover(&item.folder, false)
                    .map_err(|failure| FolderError::Failed(failure.to_string()))?
            } else {
                None
            };
            let shown = have.min(folder_cover.as_ref().map_or(have, |(_, side)| *side));
            let barcode = album
                .iter()
                .find_map(|song| song.barcode.clone().filter(|code| !code.is_empty()));

            // A picked album is looked up whatever its size: it was picked.
            if picked.is_none() && shown >= run.smaller_than {
                self.store.update(|r| r.kept += 1);
                continue;
            }

            if run.mode == CoverUpgradeMode::Scan {
                let soft = CoverUpgradeChange {
                    id: id.clone(),
                    folder: item.folder.clone(),
                    artist: first.artist.clone(),
                    album: first.album.clone(),
                    from_side: shown,
                    to_side: 0,
                    source: None,
                    files: count(album_paths.len()),
                    folder_cover: folder_cover.is_some(),
                    result: "soft".into(),
                    first_file: Some(first.path.clone()),
                    paths: Some(album_paths),
                    navidrome_album_id: item.navidrome_album_id.clone(),
                    barcode,
                    looks_same: None,
                };
                self.store.update(|r| {
                    if add_row(r, soft) {
                        r.soft += 1;
                    }
                });
                continue;
            }

            looked = true;

            let query = AlbumCoverQuery {
                artist: first.artist.clone(),
                album: first.album.clone(),
                title: first.title.clone(),
                music_brainz_release_id: album
                    .iter()
                    .find_map(|song| song.release_id.clone().filter(|id| !id.is_empty())),
                music_brainz_release_group_id: album
                    .iter()
                    .find_map(|song| song.release_group_id.clone().filter(|id| !id.is_empty())),
                barcode: barcode.clone(),
            };
            // A preview only needs to know what was found; a replace needs the picture itself.
            let lookup = async {
                if run.dry_run() {
                    self.finder.preview(&query).await
                } else {
                    self.finder.find(&query).await
                }
            };
            let found = tokio::select! {
                found = lookup => found,
                () = stopping.cancelled() => return Err(FolderError::Stopped),
            };

            let gains = |side: i32, found_side: i32| {
                f64::from(found_side) >= f64::from(side) * Self::MINIMUM_GAIN && found_side > side
            };
            let upgrade_files: Vec<&SongFile> = match &found {
                Some(found) => album
                    .iter()
                    .copied()
                    .filter(|song| gains(song.side, found.side))
                    .collect(),
                None => Vec::new(),
            };
            let mut upgrade_folder = match (&found, &folder_cover) {
                (Some(found), Some((_, side))) => gains(*side, found.side),
                _ => false,
            };
            let found = match found {
                Some(found) if !upgrade_files.is_empty() || upgrade_folder => found,
                other => {
                    // On a picked list, an album that stays as it is still says so.
                    let none = CoverUpgradeChange {
                        id: id.clone(),
                        folder: item.folder.clone(),
                        artist: first.artist.clone(),
                        album: first.album.clone(),
                        from_side: shown,
                        to_side: other.as_ref().map_or(0, |found| found.side),
                        source: other.map(|found| found.source),
                        files: 0,
                        folder_cover: false,
                        result: "none".into(),
                        first_file: Some(first.path.clone()),
                        paths: Some(album_paths),
                        navidrome_album_id: item.navidrome_album_id.clone(),
                        barcode: None,
                        looks_same: None,
                    };
                    self.store.update(|r| {
                        r.kept += 1;
                        if picked.is_some() {
                            add_row(r, none);
                        }
                    });
                    continue;
                }
            };

            self.store.save_found_thumb(
                &id,
                cover_image::to_jpeg(&cover_image::fit_within(&found.bytes, Self::THUMB_SIDE)),
            );

            // The same artwork, only sharper? Checked against the cover the album has now, so a
            // name match that found another edition, a clean version or another album entirely
            // is flagged rather than written. Unknown when the album has no cover to compare.
            let looks_now = album.iter().find_map(|song| song.looks).or_else(|| {
                folder_cover
                    .as_ref()
                    .and_then(|(own, _)| cover_image::looks_hash(std::fs::read(own).ok().as_deref()))
            });
            let looks_same = match (looks_now, cover_image::looks_hash(Some(&found.bytes))) {
                (Some(now), Some(then)) => Some(cover_image::look_alike(now, then)),
                _ => None,
            };
            if looks_same == Some(false) && picked.is_none() && !run.dry_run() {
                // Nobody picked this album, so a different picture is not written on a guess.
                self.store.update(|r| r.kept += 1);
                continue;
            }

            let mut written = 0;
            if !run.dry_run() {
                let embed = if run.full_size {
                    found.bytes.clone()
                } else {
                    cover_image::fit_within(&found.bytes, MetadataSettings::EMBEDDED_COVER_SIDE as u32)
                };
                let embed = Arc::new(embed);
                for song in &upgrade_files {
                    if stopping.is_cancelled() {
                        break;
                    }
                    if self.write_embedded(&song.path, embed.clone(), &run.run_id).await {
                        written += 1;
                    }
                }
                if upgrade_folder
                    && let Some((path, _)) = &folder_cover
                    && !self.write_folder_cover(path, &found.bytes, &run.run_id).await
                {
                    upgrade_folder = false;
                }
            } else {
                written = if item.navidrome_album_id.is_some() {
                    album_paths.len()
                } else {
                    upgrade_files.len()
                };
            }

            let change = CoverUpgradeChange {
                id: id.clone(),
                folder: item.folder.clone(),
                artist: first.artist.clone(),
                album: first.album.clone(),
                from_side: shown,
                to_side: found.side,
                source: Some(found.source.clone()),
                files: count(if run.dry_run() { album_paths.len() } else { written }),
                folder_cover: upgrade_folder,
                result: if run.dry_run() { "found" } else { "upgraded" }.into(),
                first_file: Some(first.path.clone()),
                paths: Some(album_paths),
                navidrome_album_id: item.navidrome_album_id.clone(),
                barcode,
                looks_same,
            };
            self.store.update(|r| {
                if add_row(r, change) {
                    r.upgraded += 1;
                }
                r.files += count(written);
            });
        }
        Ok(looked)
    }
}

impl CoverUpgradeWorker {
    /// Replaces the front cover and nothing else, after keeping the old one.
    async fn write_embedded(&self, path: &str, cover: Arc<Vec<u8>>, run_id: &str) -> bool {
        let (journal, target, run_id) = (self.journal.clone(), path.to_string(), run_id.to_string());
        let written = tokio::task::spawn_blocking(move || -> Result<bool, String> {
            let mut file = TagFile::open(&target).map_err(|failure| failure.to_string())?;
            let pictures = file.pictures();
            let front = front_index(&pictures);
            let before = front.map(|at| pictures[at].data.as_slice());
            if !journal.record(&target, CoverUpgradeJournal::EMBEDDED, before, &run_id) {
                return Ok(false);
            }
            let mut replaced = vec![TagPicture::front_cover(
                cover.to_vec(),
                cover_image::mime_type(&cover),
            )];
            replaced.extend(
                pictures
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| Some(*at) != front)
                    .map(|(_, picture)| picture.clone()),
            );
            file.set_pictures(&replaced);
            file.save().map_err(|failure| failure.to_string())?;
            Ok(true)
        })
        .await
        .unwrap_or_else(|panic| Err(panic.to_string()));
        self.recorded(path, written)
    }

    async fn write_folder_cover(&self, path: &str, cover: &[u8], run_id: &str) -> bool {
        let (journal, target, cover, run_id) = (
            self.journal.clone(),
            path.to_string(),
            cover.to_vec(),
            run_id.to_string(),
        );
        let written = tokio::task::spawn_blocking(move || -> Result<bool, String> {
            let before = std::fs::read(&target).map_err(|failure| failure.to_string())?;
            if !journal.record(&target, CoverUpgradeJournal::FOLDER_FILE, Some(&before), &run_id) {
                return Ok(false);
            }
            let mut bytes = cover_image::to_jpeg(&cover);
            let name = Path::new(&target)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            if eq_ignore_case(&name, cover_files::FILE_NAME) && cover_image::is_octo_cover(&before) {
                bytes = cover_image::mark_as_octo(&bytes);
            }
            write_beside(&target, &bytes).map_err(|failure| failure.to_string())?;
            Ok(true)
        })
        .await
        .unwrap_or_else(|panic| Err(panic.to_string()));
        self.recorded(path, written)
    }

    /// A write's failure goes on the run, as the C# catch put it there.
    fn recorded(&self, path: &str, written: Result<bool, String>) -> bool {
        match written {
            Ok(written) => written,
            Err(message) => {
                self.store.update(|r| {
                    r.failed += 1;
                    r.errors.push(format!("{path}: {message}"));
                });
                false
            }
        }
    }

    /// The cover file Navidrome would show for this folder, when it is a JPEG this run may
    /// replace. A PNG or WebP one is reported and left, since a JPEG under its name would lie
    /// about what it is. An error is what the C# threw (a folder or file it could not read).
    fn folder_cover(&self, folder: &str, quiet: bool) -> io::Result<Option<(String, i32)>> {
        for stem in ["cover", "folder", "front"] {
            let Some(file) = first_file_like(Path::new(folder), stem)? else {
                continue;
            };
            let extension = get_extension(&file);
            if !eq_ignore_case(extension, ".jpg") && !eq_ignore_case(extension, ".jpeg") {
                if !quiet {
                    self.store
                        .update(|r| r.errors.push(format!("{file}: not a JPEG, left as it is")));
                }
                return Ok(None);
            }
            let side =
                cover_image::measure(&std::fs::read(&file)?).map_or(0, |(width, height)| width.min(height));
            return Ok(Some((file, i32::try_from(side).unwrap_or(i32::MAX))));
        }
        Ok(None)
    }

    /// The cover Navidrome shows for an album, at the size the apps ask for, so the wall fills
    /// as fast as the apps do: Navidrome keeps these already made. None when Navidrome cannot
    /// be asked, and the dashboard then gets a copy read from the song itself. The answer is
    /// the picture and its media type.
    pub async fn navidrome_thumbnail(&self, id: &str) -> Option<(Vec<u8>, String)> {
        let album = self.store.read(|run| {
            run.preview
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.navidrome_album_id.clone())
        })?;
        let (user, token, salt) = self.services.identity.as_ref()?.get_scan_auth()?;
        let base_url = self
            .settings
            .current()
            .subsonic
            .url
            .clone()
            .filter(|url| !is_blank(url))?;
        let http = self.services.http.as_ref()?;
        let url = format!(
            "{}/rest/getCoverArt?c=octo&v=1.16.1&size=300&id={}&u={}&t={token}&s={salt}",
            base_url.trim_end_matches('/'),
            escape_data_string(&format!("al-{album}")),
            escape_data_string(&user)
        );
        let attempt = async {
            let response = http.get(&url).send().await?;
            let kind = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(';').next().unwrap_or_default().trim().to_string())
                .unwrap_or_default();
            let answer = HttpAnswer::read(response).await?;
            anyhow::Ok(
                (answer.is_success() && kind.starts_with("image/")).then(|| (answer.body.to_vec(), kind)),
            )
        };
        match attempt.await {
            Ok(answer) => answer,
            Err(failure) => {
                debug!("Navidrome cover for album {album} failed: {failure}");
                None
            }
        }
    }

    /// Which album each file belongs to, from Navidrome's own song list (a few requests for the
    /// whole library), so a scan can go album by album without reading a single file to find
    /// out. Keyed by full path. Empty when Navidrome cannot be asked; the scan then goes folder
    /// by folder as before.
    async fn navidrome_albums(&self, root: &str) -> HashMap<String, String> {
        let mut albums: HashMap<String, String> = HashMap::new();
        let base_url = self.settings.current().subsonic.url.clone();
        let (Some(identity), Some(http), Some(base_url)) = (
            self.services.identity.as_ref(),
            self.services.http.as_ref(),
            base_url.filter(|url| !is_blank(url)),
        ) else {
            return albums;
        };
        let attempt = async {
            let Some(jwt) = identity.ensure_admin_jwt().await.filter(|jwt| !jwt.is_empty()) else {
                return anyhow::Ok(());
            };
            const PAGE: usize = 1000;
            let mut start = 0;
            while start < 200_000 {
                let url = format!(
                    "{}/api/song?_start={start}&_end={}&_sort=id&_order=ASC",
                    base_url.trim_end_matches('/'),
                    start + PAGE
                );
                let answer = HttpAnswer::read(
                    http.get(&url)
                        .header("X-Nd-Authorization", format!("Bearer {jwt}"))
                        .send()
                        .await?,
                )
                .await?;
                if !answer.is_success() {
                    break;
                }
                let doc = parse_json(&answer.body)?;
                let Some(songs) = doc.as_array() else {
                    break;
                };
                for song in songs {
                    let text = |name: &str| {
                        song.get(name)
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    };
                    let (Some(album), Some(path)) = (
                        text("albumId").filter(|album| !album.is_empty()),
                        text("path").filter(|path| !path.is_empty()),
                    ) else {
                        continue;
                    };
                    let relative = path
                        .replace('\\', "/")
                        .trim_start_matches('/')
                        .split('/')
                        .filter(|segment| !segment.is_empty())
                        .collect::<Vec<_>>()
                        .join("/");
                    for base in [text("libraryPath"), Some(root.to_string())]
                        .into_iter()
                        .flatten()
                    {
                        if base.is_empty() {
                            continue;
                        }
                        albums
                            .entry(full_path(&combine(&base, &relative)))
                            .or_insert_with(|| album.clone());
                    }
                    // An older Navidrome reports the full path instead.
                    if path.starts_with('/') {
                        albums.entry(full_path(&path)).or_insert_with(|| album.clone());
                    }
                }
                if songs.len() < PAGE {
                    break;
                }
                start += PAGE;
            }
            Ok(())
        };
        if let Err(failure) = attempt.await {
            info!("Cover upgrade could not list Navidrome's songs, so it goes folder by folder: {failure}");
        }
        albums
    }

    /// Files grouped into one item per Navidrome album where Navidrome named one, and per
    /// folder for the rest.
    pub fn by_album(files: &[String], albums: &HashMap<String, String>) -> Vec<CoverUpgradeItem> {
        let mut named: Vec<(&str, Vec<String>)> = Vec::new();
        let mut rest: Vec<String> = Vec::new();
        for path in files {
            match albums.get(&full_path(path)) {
                Some(album) => match named.iter_mut().find(|(known, _)| known == album) {
                    Some((_, list)) => list.push(path.clone()),
                    None => named.push((album.as_str(), vec![path.clone()])),
                },
                None => rest.push(path.clone()),
            }
        }
        let mut items: Vec<CoverUpgradeItem> = named
            .into_iter()
            .map(|(album, mut list)| {
                list.sort_by(|a, b| compare_ordinal(a, b));
                CoverUpgradeItem::new(directory_name(&list[0]), Some(list), Some(album.to_string()))
            })
            .collect();
        let mut folders: Vec<(String, Vec<String>)> = Vec::new();
        for path in rest {
            let folder = directory_name(&path);
            match folders.iter_mut().find(|(known, _)| *known == folder) {
                Some((_, list)) => list.push(path),
                None => folders.push((folder, vec![path])),
            }
        }
        items.extend(folders.into_iter().map(|(folder, mut list)| {
            list.sort_by(|a, b| compare_ordinal(a, b));
            CoverUpgradeItem::new(folder, Some(list), None)
        }));
        let first = |item: &CoverUpgradeItem| -> String {
            item.files
                .as_ref()
                .and_then(|files| files.first())
                .unwrap_or(&item.folder)
                .clone()
        };
        items.sort_by(|a, b| compare_ordinal(&first(a), &first(b)));
        items
    }

    /// Undo (`UndoAsync`): puts back every journalled picture.
    async fn undo(&self, stopping: &CancellationToken) -> anyhow::Result<()> {
        let entries = self.journal.read_all();
        self.store.replace(CoverUpgradeRun {
            run_id: new_run_id(),
            status: CoverUpgradeStatus::Running,
            mode: CoverUpgradeMode::Apply,
            undo: true,
            started_utc: Some(Utc::now()),
            total: count(entries.len()),
            reason: Some("Putting back the covers the last run replaced.".to_string()),
            ..CoverUpgradeRun::default()
        });

        // Newest first, so a file changed by two runs ends with the cover the oldest one found.
        let mut left: Vec<CoverUpgradeJournalEntry> = Vec::new();
        let mut restored = 0;
        for entry in entries {
            if stopping.is_cancelled() || self.cancel.load(Ordering::SeqCst) {
                left.push(entry);
                continue;
            }
            if Path::new(&entry.path).is_file() {
                let (journal, job) = (self.journal.clone(), entry.clone());
                let put_back = tokio::task::spawn_blocking(move || restore(&journal, &job))
                    .await
                    .unwrap_or_else(|panic| Err(panic.to_string()));
                match put_back {
                    Ok(()) => {
                        restored += 1;
                        self.store.update(|r| r.files += 1);
                    }
                    Err(message) => {
                        self.store.update(|r| {
                            r.failed += 1;
                            r.errors.push(format!("{}: {message}", entry.path));
                        });
                        left.push(entry.clone());
                    }
                }
            } else {
                left.push(entry.clone());
            }
            self.store.update(|r| {
                r.processed += 1;
                r.last_folder = Some(directory_name(&entry.path));
            });
        }

        left.reverse();
        self.journal.rewrite(&left)?;
        self.store.update(|r| {
            r.status = if left.is_empty() {
                CoverUpgradeStatus::Completed
            } else {
                CoverUpgradeStatus::Cancelled
            };
            r.finished_utc = Some(Utc::now());
            r.reason = Some(if left.is_empty() {
                format!("Put back the old cover on {restored} file(s).")
            } else {
                format!(
                    "Put back {restored} file(s). {} are still to do (missing, unreadable or not reached); Undo again once they are back.",
                    left.len()
                )
            });
        });
        if restored > 0 {
            self.rescan().await;
        }
        Ok(())
    }

    /// The work of a run over everything in scope (`EnumerateAsync`).
    async fn enumerate(&self, scope: CoverUpgradeScope) -> anyhow::Result<Vec<CoverUpgradeItem>> {
        let fallback = self
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "/music".to_string());
        let root = match &self.services.identity {
            Some(identity) => identity.effective_download_path(&fallback),
            None => fallback,
        };
        let files: Vec<String> = if scope == CoverUpgradeScope::OctoDownloads {
            let library = self
                .services
                .library
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!(NO_LIBRARY_SERVICE))?;
            let mut seen = HashSet::new();
            library
                .get_mappings()
                .await
                .into_iter()
                .map(|mapping| mapping.local_path)
                .filter(|path| !path.is_empty() && Path::new(path).is_file())
                .filter(|path| seen.insert(path.clone()))
                .collect()
        } else if !Path::new(&root).is_dir() {
            warn!("Cover upgrade found no music folder at {root}");
            return Ok(Vec::new());
        } else {
            let walk_root = PathBuf::from(&root);
            tokio::task::spawn_blocking(move || all_files(&walk_root))
                .await
                .map_err(|panic| anyhow::anyhow!(panic.to_string()))??
                .into_iter()
                .filter(|path| is_audio(path))
                .collect()
        };
        let albums = self.navidrome_albums(&root).await;
        info!(
            "Cover upgrade: Navidrome named the album of {} of {} song(s)",
            files
                .iter()
                .filter(|path| albums.contains_key(&full_path(path)))
                .count(),
            files.len()
        );
        Ok(Self::by_album(&files, &albums))
    }

    async fn rescan(&self) {
        match &self.services.library {
            Some(library) => {
                library.trigger_library_scan(true).await;
            }
            None => warn!("Cover upgrade could not ask Navidrome to scan: {NO_LIBRARY_SERVICE}"),
        }
    }

    /// `BarcodeOf`: the album's barcode when the song carries one.
    pub fn barcode_of(file: &TagFile) -> Option<String> {
        octo_media::tags::tag_writer_extras::barcode_of(file)
    }
}

/// Puts one journalled picture back (the body of the C# undo loop).
fn restore(journal: &CoverUpgradeJournal, entry: &CoverUpgradeJournalEntry) -> Result<(), String> {
    let before = journal.backup(entry.hash.as_deref());
    if entry.hash.is_some() && before.is_none() {
        return Err("its kept cover is missing".to_string());
    }
    if entry.kind == CoverUpgradeJournal::FOLDER_FILE {
        // File.WriteAllBytes(path, null) threw for a folder file kept without a picture.
        let before = before.ok_or_else(|| "Value cannot be null. (Parameter 'bytes')".to_string())?;
        return write_beside(&entry.path, &before).map_err(|failure| failure.to_string());
    }
    let mut file = TagFile::open(&entry.path).map_err(|failure| failure.to_string())?;
    let pictures = file.pictures();
    let front = front_index(&pictures);
    let rest = pictures
        .iter()
        .enumerate()
        .filter(|(at, _)| Some(*at) != front)
        .map(|(_, picture)| picture.clone());
    let restored: Vec<TagPicture> = match before {
        None => rest.collect(),
        Some(before) => {
            let mime = cover_image::mime_type(&before);
            std::iter::once(TagPicture::front_cover(before, mime))
                .chain(rest)
                .collect()
        }
    };
    file.set_pictures(&restored);
    file.save().map_err(|failure| failure.to_string())
}

/// `<path>.octo-tmp` written and moved over the file.
fn write_beside(path: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = format!("{path}.octo-tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

/// Lists an album once. An album already listed (a resumed batch runs again) takes its new
/// row's place and is not counted twice. True when it is new.
fn add_row(run: &mut CoverUpgradeRun, row: CoverUpgradeChange) -> bool {
    if let Some(existing) = run.preview.iter_mut().find(|existing| existing.id == row.id) {
        *existing = row;
        return false;
    }
    if run.preview.len() < CoverUpgradeStore::MAX_PREVIEW_ROWS {
        run.preview.push(row);
    }
    true
}

/// One song as a run reads it (`ReadSong`), or None when it names no artist, or neither an
/// album nor a title, or cannot be read.
fn read_song(path: &str) -> Option<SongFile> {
    // Tags and pictures only: the audio properties cost extra reads, which over a network mount
    // is most of the time a scan takes.
    let file = match TagFile::open(path) {
        Ok(file) => file,
        Err(failure) => {
            debug!("Cover upgrade could not read {path}: {failure}");
            return None;
        }
    };
    let artist = if octo_media::tags::tag_writer_extras::is_compilation(&file) {
        Some("Various Artists".to_string())
    } else {
        file.first_album_artist()
            .filter(|artist| !is_blank(artist))
            .or_else(|| file.first_performer())
    };
    let (album, title) = (file.album(), file.title());
    let artist = artist.filter(|artist| !is_blank(artist))?;
    if is_null_or_white_space(album.as_deref()) && is_null_or_white_space(title.as_deref()) {
        return None;
    }
    let bytes = front_of(&file.pictures()).map(|picture| picture.data);
    let side = bytes
        .as_deref()
        .filter(|bytes| !bytes.is_empty())
        .and_then(cover_image::measure)
        .map_or(0, |(width, height)| {
            i32::try_from(width.min(height)).unwrap_or(i32::MAX)
        });
    Some(SongFile {
        path: path.to_string(),
        artist,
        album,
        title,
        release_id: file.music_brainz_release_id(),
        release_group_id: file.music_brainz_release_group_id(),
        side,
        barcode: octo_media::tags::tag_writer_extras::barcode_of(&file),
        looks: if side > 0 {
            cover_image::looks_hash(bytes.as_deref())
        } else {
            None
        },
    })
}

/// `FrontOf`: the front cover, else the first picture.
fn front_index(pictures: &[TagPicture]) -> Option<usize> {
    pictures
        .iter()
        .position(|picture| picture.picture_type == FRONT_COVER)
        .or(if pictures.is_empty() { None } else { Some(0) })
}

fn front_of(pictures: &[TagPicture]) -> Option<TagPicture> {
    front_index(pictures).map(|at| pictures[at].clone())
}

/// True when every song in the folder is one of these: a cover.jpg there is this album's
/// alone.
fn folder_holds_only(folder: &str, album_paths: &[String]) -> bool {
    let mine: HashSet<&str> = album_paths.iter().map(String::as_str).collect();
    audio_files_in(Path::new(folder)).is_ok_and(|files| files.iter().all(|path| mine.contains(path.as_str())))
}

/// `Directory.EnumerateFiles(folder)` kept to the audio extensions.
fn audio_files_in(folder: &Path) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let path = entry?.path();
        if path.is_file() {
            let path = path.to_string_lossy().into_owned();
            if is_audio(&path) {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// `Directory.EnumerateFiles(folder, "<stem>.*").FirstOrDefault()`. The .NET pattern is
/// case-sensitive on Linux and, being a Win32 pattern, also matches the bare stem.
fn first_file_like(folder: &Path, stem: &str) -> io::Result<Option<String>> {
    let prefix = format!("{stem}.");
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name == stem || name.starts_with(&prefix)) && entry.path().is_file() {
            return Ok(Some(entry.path().to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

fn is_audio(path: &str) -> bool {
    let extension = get_extension(path);
    CoverUpgradeWorker::AUDIO_EXTENSIONS
        .iter()
        .any(|audio| eq_ignore_case(audio, extension))
}

/// `Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)`. Symbolic links to folders
/// are followed, and any folder that cannot be read fails the whole listing.
fn all_files(root: &Path) -> io::Result<Vec<String>> {
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
            } else {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    Ok(found)
}

/// `Path.GetExtension`: from the last dot of the file name, or nothing when there is no dot or
/// the name ends in one.
fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

/// `Path.GetDirectoryName`, or "" for a bare name.
fn directory_name(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `Path.Combine(base, relative)`.
fn combine(base: &str, relative: &str) -> String {
    if relative.is_empty() {
        base.to_string()
    } else if base.ends_with('/') {
        format!("{base}{relative}")
    } else {
        format!("{base}/{relative}")
    }
}

/// `Path.GetFullPath`: made absolute against the working folder, with `.`, `..` and doubled
/// separators taken out.
fn full_path(path: &str) -> String {
    let absolute = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let mut full = format!("/{}", parts.join("/"));
    if path.ends_with('/') && full.len() > 1 {
        full.push('/');
    }
    full
}

/// The parent folder of a state file (`Path.GetDirectoryName`), "." for a bare name.
fn parent_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// A list's length or index as the run's `int` counters hold it.
fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// `StringComparer.Ordinal`: UTF-16 code unit order.
fn compare_ordinal(a: &str, b: &str) -> CmpOrdering {
    a.encode_utf16().cmp(b.encode_utf16())
}

#[cfg(test)]
#[path = "cover_upgrade_tests.rs"]
mod tests;
