//! Port of `Services/Metadata/GenreBackfillWorker.cs`: the genre backfill, a singleton that is
//! also a hosted queue worker. The run and its store are `genre_backfill_state`, the undo log
//! `genre_backfill_journal`, and the rules `octo_core::metadata::GenreNormalizer`.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use octo_core::common::dotnet::ordinal_ignore_case_key;
use octo_core::metadata::{GenreNormalizer, GenreTagAction};
use octo_core::settings::{GenreSettings, SettingsStore};
use octo_media::tags::{TagError, TagFile};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::genre_backfill_journal::{GenreBackfillJournal, GenreJournalEntry};
use super::genre_backfill_state::{
    GenreBackfillChange, GenreBackfillRun, GenreBackfillScope, GenreBackfillStatus, GenreBackfillStore,
};
use crate::services::local::ILocalLibraryService;

/// What the dashboard asks for: a walk over `scope` (a preview when `dry_run`), or an undo of
/// the journal (`undo`, which ignores the other two).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenreBackfillRequest {
    pub scope: GenreBackfillScope,
    pub dry_run: bool,
    pub undo: bool,
}

impl GenreBackfillRequest {
    /// `new GenreBackfillRequest(scope, dryRun)`.
    pub fn new(scope: GenreBackfillScope, dry_run: bool) -> Self {
        Self {
            scope,
            dry_run,
            undo: false,
        }
    }

    /// The undo request (`new GenreBackfillRequest(WholeLibrary, DryRun: false, Undo: true)`).
    pub fn undo() -> Self {
        Self {
            scope: GenreBackfillScope::WholeLibrary,
            dry_run: false,
            undo: true,
        }
    }
}

/// What the C# `GetRequiredService<ILocalLibraryService>()` threw when the container had none
/// (the tests' empty service provider).
const NO_LIBRARY_SERVICE: &str =
    "No service for type 'Octo.Services.Local.ILocalLibraryService' has been registered.";

/// Re-tags genres across files that are already in the library.
///
/// A BackgroundService rather than a request, because a 1,900-file walk does not fit in one
/// and the shutdown budget is ten seconds. One run at a time: a second request gets 409 rather
/// than a parallel walk over the same files.
pub struct GenreBackfillWorker {
    store: Arc<GenreBackfillStore>,
    journal: Arc<GenreBackfillJournal>,
    settings: Arc<SettingsStore>,
    /// Resolved from a scope in C#; None stands for a container without one (the C# tests).
    library: Option<Arc<dyn ILocalLibraryService>>,
    cancel_requested: AtomicBool,

    // True from the moment a request is accepted until the worker has finished with it. The
    // store only says Running once enumeration is done, which on a large library is minutes;
    // without this a second request in that window was accepted and ran afterwards, unconfirmed.
    pending: AtomicBool,
    sender: mpsc::Sender<GenreBackfillRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<GenreBackfillRequest>>,
}

enum FileOutcome {
    Unchanged,
    Changed(GenreBackfillChange),
    Skipped,
    Failed(String),
}

impl GenreBackfillWorker {
    /// The early-failure stop condition. The consecutive-failure ceiling is a setting, because
    /// how much a user tolerates before giving up is a preference; this ratio is a sanity check
    /// on the first sample and is not worth a knob.
    const EARLY_SAMPLE_SIZE: i32 = 200;
    const EARLY_FAILURE_RATIO: f64 = 0.10;

    pub fn new(
        store: Arc<GenreBackfillStore>,
        journal: Arc<GenreBackfillJournal>,
        settings: Arc<SettingsStore>,
        library: Option<Arc<dyn ILocalLibraryService>>,
    ) -> Self {
        // Capacity 1, and a request past it is dropped (`DropWrite`): `pending` keeps a second
        // one from ever being written while one waits.
        let (sender, receiver) = mpsc::channel(1);
        Self {
            store,
            journal,
            settings,
            library,
            cancel_requested: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
        }
    }

    pub fn is_running(&self) -> bool {
        self.store.read(|run| run.status == GenreBackfillStatus::Running)
    }

    /// The run the dashboard polls. Exposed here so the controller has one thing to talk to
    /// rather than needing the store as well.
    pub fn current(&self) -> GenreBackfillRun {
        self.store.current()
    }

    /// The journal, for the controller's "can undo" (`Exists`).
    pub fn journal(&self) -> &Arc<GenreBackfillJournal> {
        &self.journal
    }

    /// False when a run is already going or waiting to start, which the controller turns into
    /// a 409. A full bounded channel in DropWrite mode reports a dropped write as a success, so
    /// the channel alone could not say no.
    pub fn try_enqueue(&self, request: GenreBackfillRequest) -> bool {
        if self.is_running()
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

    /// A short, stable fingerprint of the genre settings a run is planned from.
    ///
    /// Only what decides a file's new genre and which files are walked: the failure ceiling is
    /// how long a run keeps trying, not what it writes. The JSON is System.Text.Json's for the
    /// C# anonymous object (PascalCase, enums as numbers, its escaping), so a run recorded by
    /// the C# build still resumes.
    pub fn hash_settings(settings: &GenreSettings) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Mapping<'a> {
            id: &'a str,
            pattern: &'a str,
            genre: &'a str,
            #[serde(rename = "Match")]
            match_mode: i32,
            enabled: bool,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Hashed<'a> {
            enabled: bool,
            mappings: Vec<Mapping<'a>>,
            blocklist: &'a [String],
            fallback: i32,
            max_genres: i32,
            on_empty: i32,
            unknown_label: &'a str,
            backfill_extensions: &'a [String],
        }
        let hashed = Hashed {
            enabled: settings.enabled,
            mappings: settings
                .mappings
                .iter()
                .map(|mapping| Mapping {
                    id: &mapping.id,
                    pattern: &mapping.pattern,
                    genre: &mapping.genre,
                    match_mode: mapping.match_mode as i32,
                    enabled: mapping.enabled,
                })
                .collect(),
            blocklist: &settings.blocklist,
            fallback: settings.fallback as i32,
            max_genres: settings.max_genres,
            on_empty: settings.on_empty as i32,
            unknown_label: &settings.unknown_label,
            backfill_extensions: &settings.backfill_extensions,
        };
        let json = octo_core::json::to_string(&hashed);
        hex::encode_upper(Sha256::digest(json.as_bytes()))[..16].to_string()
    }

    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
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
            // Per-item catch is mandatory: one unhandled error here would take the worker
            // down (in C#, BackgroundServiceExceptionBehavior.StopHost took Octo down).
            let result = if request.undo {
                self.run_undo(&stopping).await
            } else {
                self.run_request(request, &stopping).await
            };
            if let Err(failure) = result {
                error!("Genre backfill failed: {failure:#}");
                self.store.update(|run| {
                    run.status = GenreBackfillStatus::Failed;
                    run.reason = Some(failure.to_string());
                    run.finished_utc = Some(Utc::now());
                });
            }
            self.pending.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    /// One walk (`RunAsync`).
    async fn run_request(
        &self,
        request: GenreBackfillRequest,
        stopping: &CancellationToken,
    ) -> anyhow::Result<()> {
        self.cancel_requested.store(false, Ordering::SeqCst);

        // Snapshot the settings at run start rather than reading per file, so a run's results
        // are explainable: a mid-run settings edit would otherwise produce a file where half
        // the library followed one table and half followed another.
        let settings = Arc::new(self.settings.current().genre.clone());

        // Resuming continues a run under the settings it started with; after a rules edit the
        // only honest option is a fresh run.
        let hash = Self::hash_settings(&settings);
        let resuming = self.store.read(|run| {
            request.scope == run.scope
                && request.dry_run == run.dry_run
                && run.can_resume()
                && run.settings_hash.as_ref().is_none_or(|known| *known == hash)
        });

        let queue: Vec<String>;
        if resuming {
            queue = self.store.read(|run| run.queue.clone());
            self.store.update(|run| {
                run.status = GenreBackfillStatus::Running;
                run.reason = None;
            });
            info!(
                "Genre backfill resuming at {}/{}",
                self.store.read(|run| run.cursor),
                queue.len()
            );
        } else {
            queue = self.enumerate(request.scope).await?;
            self.store.replace(GenreBackfillRun {
                run_id: new_run_id(),
                status: GenreBackfillStatus::Running,
                scope: request.scope,
                dry_run: request.dry_run,
                started_utc: Some(Utc::now()),
                total: count(queue.len()),
                queue: queue.clone(),
                settings_hash: Some(hash),
                ..GenreBackfillRun::default()
            });
            info!(
                "Genre backfill started: {} file(s), scope {:?}, dryRun {}",
                queue.len(),
                request.scope,
                if request.dry_run { "True" } else { "False" }
            );
        }

        let (run_id, dry_run, start) = self
            .store
            .read(|run| (run.run_id.clone(), run.dry_run, run.cursor));
        let mut consecutive_failures = 0;

        for (index, path) in queue.iter().enumerate().skip(start.max(0) as usize) {
            // Checked between files, never mid-file: a half-written tag block is a corrupt file.
            if stopping.is_cancelled() {
                self.store
                    .update(|run| run.status = GenreBackfillStatus::Interrupted);
                return Ok(());
            }
            if self.cancel_requested.load(Ordering::SeqCst) {
                self.store.update(|run| {
                    run.status = GenreBackfillStatus::Cancelled;
                    run.finished_utc = Some(Utc::now());
                    run.reason = Some("Cancelled from the dashboard.".to_string());
                });
                info!("Genre backfill cancelled at {index}/{}", queue.len());
                return Ok(());
            }

            let outcome = {
                let (path, settings, journal, run_id) = (
                    path.clone(),
                    settings.clone(),
                    self.journal.clone(),
                    run_id.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    process_file(&path, &settings, dry_run, &run_id, &journal)
                })
                .await
                .unwrap_or_else(|panic| FileOutcome::Failed(panic.to_string()))
            };
            let failed = matches!(outcome, FileOutcome::Failed(_));

            self.store.update(|run| {
                run.cursor = count(index) + 1;
                run.processed += 1;
                run.last_path = Some(path.clone());
                match outcome {
                    FileOutcome::Changed(change) => {
                        run.changed += 1;
                        if change.action == "Clear" {
                            run.cleared += 1;
                        }
                        if run.preview.len() < GenreBackfillStore::MAX_PREVIEW_ROWS {
                            run.preview.push(change);
                        }
                    }
                    FileOutcome::Skipped => run.skipped += 1,
                    FileOutcome::Failed(error) => {
                        run.failed += 1;
                        run.errors.push(format!("{path}: {error}"));
                    }
                    FileOutcome::Unchanged => {}
                }
            });

            consecutive_failures = if failed { consecutive_failures + 1 } else { 0 };

            let (processed, failed_so_far) = self.store.read(|run| (run.processed, run.failed));
            let early_ratio_breached = processed >= Self::EARLY_SAMPLE_SIZE
                && f64::from(failed_so_far) >= f64::from(processed) * Self::EARLY_FAILURE_RATIO
                && processed <= Self::EARLY_SAMPLE_SIZE * 2;

            let failure_ceiling = settings.effective_backfill_max_consecutive_failures();
            let ceiling_reached = failure_ceiling > 0 && consecutive_failures >= failure_ceiling;
            if ceiling_reached || early_ratio_breached {
                let reason = if ceiling_reached {
                    format!(
                        "{consecutive_failures} files in a row could not be written. Is the music directory read-only?"
                    )
                } else {
                    format!("{failed_so_far} of the first {processed} files could not be written.")
                };
                error!("Genre backfill stopped: {reason}");
                self.store.update(|run| {
                    run.status = GenreBackfillStatus::Failed;
                    run.finished_utc = Some(Utc::now());
                    run.reason = Some(reason);
                });
                return Ok(());
            }

            // Yield so a long walk does not monopolise the thread pool.
            if index % 50 == 49 {
                tokio::task::yield_now().await;
            }
        }

        self.store.update(|run| {
            run.status = GenreBackfillStatus::Completed;
            run.finished_utc = Some(Utc::now());
        });

        let last = self.store.current();
        info!(
            "Genre backfill {}: {} changed ({} cleared), {} skipped, {} failed, of {}",
            if dry_run { "preview finished" } else { "finished" },
            last.changed,
            last.cleared,
            last.skipped,
            last.failed,
            last.total
        );

        if !dry_run && last.changed > 0 {
            self.rescan().await;
        }
        Ok(())
    }

    /// Undo (`RunUndoAsync`): puts every journalled file's genre frame back.
    async fn run_undo(&self, stopping: &CancellationToken) -> anyhow::Result<()> {
        self.cancel_requested.store(false, Ordering::SeqCst);

        let entries = self.journal.read_all();
        self.store.replace(GenreBackfillRun {
            run_id: new_run_id(),
            status: GenreBackfillStatus::Running,
            scope: GenreBackfillScope::WholeLibrary,
            dry_run: false,
            started_utc: Some(Utc::now()),
            total: count(entries.len()),
            reason: Some("Undoing the last genre backfill.".to_string()),
            ..GenreBackfillRun::default()
        });
        info!("Genre backfill undo started: {} entries", entries.len());

        // Newest first. A file changed twice is restored to the frame the OLDEST entry saw,
        // because that one is applied last.
        let mut restored = 0;
        let mut stopped_early = false;
        let mut outcomes: HashMap<usize, Option<bool>> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (index, entry) in entries.iter().enumerate() {
            if stopping.is_cancelled() || self.cancel_requested.load(Ordering::SeqCst) {
                stopped_early = true;
                break;
            }

            if !Path::new(&entry.path).is_file() {
                // Keyed by path, so a file moved since the run cannot be found. Kept in the
                // journal: a music folder on a mount that is down right now looks exactly like
                // this, and dropping the entries would lose the undo for all of it.
                outcomes.insert(index, None);
                self.store.update(|run| run.skipped += 1);
            } else {
                let (path, before) = (entry.path.clone(), entry.before.clone());
                let written = tokio::task::spawn_blocking(move || -> Result<(), TagError> {
                    let mut file = TagFile::open(&path)?;
                    file.set_genres(&before);
                    file.save()
                })
                .await
                .map_err(|panic| panic.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
                match written {
                    Ok(()) => {
                        restored += 1;
                        outcomes.insert(index, Some(true));
                        if seen.insert(ordinal_ignore_case_key(&entry.path)) {
                            self.store.update(|run| run.changed += 1);
                        }
                    }
                    Err(message) => {
                        outcomes.insert(index, Some(false));
                        self.store.update(|run| {
                            run.failed += 1;
                            run.errors.push(format!("{}: {message}", entry.path));
                        });
                    }
                }
            }
            self.store.update(|run| {
                run.processed += 1;
                run.last_path = Some(entry.path.clone());
            });
        }

        // Only what was actually put back leaves the journal. Clearing it outright here used to
        // throw away the undo for every file a cancel, a shutdown or an unreadable file left
        // behind.
        let remaining = Self::remaining(&entries, &outcomes);
        if remaining.is_empty() {
            self.journal.clear();
        } else {
            self.journal.rewrite(&remaining);
        }

        self.store.update(|run| {
            run.status = if stopped_early {
                GenreBackfillStatus::Cancelled
            } else {
                GenreBackfillStatus::Completed
            };
            run.finished_utc = Some(Utc::now());
            run.reason = Some(if remaining.is_empty() {
                format!("Restored the genre frame on {restored} file(s).")
            } else {
                format!(
                    "Restored {restored} file(s). {} not restored yet (missing, unreadable or not reached); \
                     they stay in the journal, so Undo again once they are back.",
                    remaining.len()
                )
            });
        });
        info!(
            "Genre backfill undo finished: {restored} restored, {} left in the journal",
            remaining.len()
        );

        if restored > 0 {
            self.rescan().await;
        }
        Ok(())
    }

    /// The journal entries an undo has not dealt with, oldest first, ready to be written back.
    ///
    /// `outcomes` is keyed by index into `newest_first`: `Some(true)` restored, `Some(false)`
    /// failed, `None` skipped; an index with no entry was never reached. A restore supersedes
    /// every NEWER entry for the same file, because the older entry's before-state is the
    /// original; keeping a newer one that had failed would put an in-between genre back on the
    /// next undo.
    pub fn remaining(
        newest_first: &[GenreJournalEntry],
        outcomes: &HashMap<usize, Option<bool>>,
    ) -> Vec<GenreJournalEntry> {
        let mut keep: Vec<GenreJournalEntry> = Vec::new();
        for (index, entry) in newest_first.iter().enumerate() {
            if outcomes.get(&index) == Some(&Some(true)) {
                // Exact: journal paths come from the same walk, and on Linux a.flac and A.flac
                // are different files.
                keep.retain(|newer| newer.path != entry.path);
            } else {
                keep.push(entry.clone());
            }
        }
        keep.reverse();
        keep
    }

    /// The files a run walks (`EnumerateAsync`), in ordinal order.
    async fn enumerate(&self, scope: GenreBackfillScope) -> anyhow::Result<Vec<String>> {
        if scope == GenreBackfillScope::OctoDownloads {
            let library = self
                .library
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!(NO_LIBRARY_SERVICE))?;
            let mut seen = HashSet::new();
            let mut paths: Vec<String> = library
                .get_mappings()
                .await
                .into_iter()
                .map(|mapping| mapping.local_path)
                .filter(|path| !path.is_empty() && Path::new(path).is_file())
                .filter(|path| seen.insert(ordinal_ignore_case_key(path)))
                .collect();
            paths.sort_by(|a, b| compare_ordinal(a, b));
            return Ok(paths);
        }

        let extensions = self.settings.current().genre.effective_backfill_extensions();
        let root = self
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        if !Path::new(&root).is_dir() {
            warn!("Genre backfill found no music directory at {root}");
            return Ok(Vec::new());
        }

        let walked = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || all_files(Path::new(&root)))
                .await
                .unwrap_or_else(|panic| Err(io::Error::other(panic.to_string())))
        };
        match walked {
            Ok(files) => {
                let mut files: Vec<String> = files
                    .into_iter()
                    .filter(|path| extensions.contains(get_extension(path)))
                    .collect();
                files.sort_by(|a, b| compare_ordinal(a, b));
                Ok(files)
            }
            Err(failure) => {
                error!("Genre backfill could not enumerate {root}: {failure}");
                Ok(Vec::new())
            }
        }
    }

    /// force: true is not optional. The scan is debounced, and a run that finishes inside the
    /// debounce window would leave every rewrite invisible to Navidrome, which reads as the
    /// button having done nothing.
    async fn rescan(&self) {
        match &self.library {
            Some(library) => {
                library.trigger_library_scan(true).await;
            }
            None => warn!("Genre backfill could not trigger a library scan: {NO_LIBRARY_SERVICE}"),
        }
    }
}

/// One file (`ProcessFile`): what the rules would write, and the write itself unless a preview.
fn process_file(
    path: &str,
    settings: &GenreSettings,
    dry_run: bool,
    run_id: &str,
    journal: &GenreBackfillJournal,
) -> FileOutcome {
    let mut file = match TagFile::open(path) {
        Ok(file) => file,
        Err(failure) => return failure_outcome(path, failure),
    };
    let before = file.genres();

    // No resolved genre and no fallback: a backfill is local-only by design, so a run over two
    // thousand files makes no network calls at all.
    let plan = GenreNormalizer::plan(&before, None, settings, None);
    if plan.action == GenreTagAction::None {
        return FileOutcome::Unchanged;
    }

    let after = if plan.action == GenreTagAction::Clear {
        Vec::new()
    } else {
        plan.genres
    };
    if before == after {
        return FileOutcome::Unchanged;
    }

    let change = GenreBackfillChange::new(
        path,
        before.clone(),
        after.clone(),
        if plan.action == GenreTagAction::Clear {
            "Clear"
        } else {
            "Write"
        },
        plan.matched_rule,
    );

    if dry_run {
        return FileOutcome::Changed(change);
    }

    // Journal BEFORE the write. A crash between the two costs an undo entry for a file that
    // was not changed, which is harmless; the other order loses the only record of a file that
    // WAS.
    journal.append(&GenreJournalEntry::new(
        path,
        before,
        after.clone(),
        Utc::now(),
        run_id,
    ));

    file.set_genres(&after);
    match file.save() {
        Ok(()) => FileOutcome::Changed(change),
        Err(failure) => failure_outcome(path, failure),
    }
}

/// The C# catch clauses: a file TagLib could not make sense of is skipped, an I/O failure
/// counts as failed, and anything else is failed with a warning.
fn failure_outcome(path: &str, failure: TagError) -> FileOutcome {
    match failure {
        TagError::Io(_) => FileOutcome::Failed(failure.to_string()),
        TagError::Read(ref read) if is_io_failure(read) => FileOutcome::Failed(failure.to_string()),
        // A cue sheet, a weird container, a stream TagLib does not model. Not an error.
        TagError::Unsupported(_) | TagError::Read(_) => {
            debug!("Genre backfill skipped {path}: {failure}");
            FileOutcome::Skipped
        }
        other => {
            warn!("Genre backfill could not process {path}: {other}");
            FileOutcome::Failed(other.to_string())
        }
    }
}

/// A read that failed for an I/O reason (a folder, a file gone or locked) rather than because
/// the bytes made no sense: TagLib threw an `IOException` or `UnauthorizedAccessException` for
/// those, and `CorruptFileException` (a short file included) for the rest.
fn is_io_failure(read: &lofty::error::FileParseError) -> bool {
    std::error::Error::source(read)
        .and_then(|source| source.downcast_ref::<io::Error>())
        .is_some_and(|error| error.kind() != io::ErrorKind::UnexpectedEof)
}

fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// A list's length or index as the run's `int` counters hold it.
fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// `StringComparer.Ordinal`: UTF-16 code unit order, which differs from Rust's (code point)
/// order above U+FFFF.
fn compare_ordinal(a: &str, b: &str) -> CmpOrdering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Path.GetExtension`: from the last dot of the file name, or nothing when there is no dot or
/// the name ends in one. (Unlike Rust's, ".mp3" has the extension ".mp3".)
fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

/// `Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)`. Symbolic links to folders
/// are followed, and any folder that cannot be read fails the whole listing
/// (`IgnoreInaccessible` is off for that overload).
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

#[cfg(test)]
#[path = "genre_backfill_worker_tests.rs"]
mod tests;
