//! Port of `Services/Metadata/GenreBackfillState.cs`: the backfill run and its store
//! (`genre-backfill.json`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::is_blank;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::services::state_file::{self, null_as_default};

/// Which files a run touches. A REQUEST parameter, never a setting, so "the whole library"
/// can never be left switched on and fire again later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum GenreBackfillScope {
    /// Only files Octo downloaded, from .mappings.json. Rewriting these is rewriting
    /// Octo's own output.
    #[default]
    OctoDownloads = 0,

    /// Every audio file under the music root, including rips and purchases Octo never
    /// touched. Gated harder for that reason.
    WholeLibrary = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum GenreBackfillStatus {
    #[default]
    Idle = 0,
    Running = 1,
    Completed = 2,
    Cancelled = 3,

    /// The process stopped mid-run. It does NOT auto-resume: a user may have
    /// restarted specifically to stop it.
    Interrupted = 4,

    /// Too many files in a row could not be written, so the run gave up rather than
    /// producing the same error two thousand times.
    Failed = 5,
}

/// One file the run would change, or did. The preview is built from these.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct GenreBackfillChange {
    #[serde(deserialize_with = "null_as_default")]
    pub path: String,
    #[serde(deserialize_with = "null_as_default")]
    pub before: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub after: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub action: String,
    pub rule: Option<String>,
}

impl GenreBackfillChange {
    pub fn new(
        path: impl Into<String>,
        before: Vec<String>,
        after: Vec<String>,
        action: impl Into<String>,
        rule: Option<String>,
    ) -> Self {
        Self {
            path: path.into(),
            before,
            after,
            action: action.into(),
            rule,
        }
    }
}

/// The run as `genre-backfill.json` holds it, with the computed `CanResume` written last.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct GenreBackfillRun {
    #[serde(deserialize_with = "null_as_default")]
    pub run_id: String,
    pub status: GenreBackfillStatus,
    pub scope: GenreBackfillScope,
    pub dry_run: bool,
    #[serde(with = "octo_core::json::datetime::utc_option")]
    pub started_utc: Option<DateTime<Utc>>,
    #[serde(with = "octo_core::json::datetime::utc_option")]
    pub finished_utc: Option<DateTime<Utc>>,
    pub total: i32,
    pub processed: i32,
    pub changed: i32,
    pub cleared: i32,
    pub skipped: i32,
    pub failed: i32,
    pub cursor: i32,
    pub last_path: Option<String>,
    pub reason: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub errors: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub preview: Vec<GenreBackfillChange>,

    /// Which genre settings this run used (GenreBackfillWorker.HashSettings). Apply
    /// re-plans from the settings in force when it runs, so a preview is only a true description
    /// of an apply while these still match. None on runs recorded before this existed.
    pub settings_hash: Option<String>,

    /// The queue this run walks, persisted so a resume does not re-enumerate into a
    /// different order and skip files.
    #[serde(deserialize_with = "null_as_default")]
    pub queue: Vec<String>,
}

impl Default for GenreBackfillRun {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            status: GenreBackfillStatus::Idle,
            scope: GenreBackfillScope::OctoDownloads,
            dry_run: true,
            started_utc: None,
            finished_utc: None,
            total: 0,
            processed: 0,
            changed: 0,
            cleared: 0,
            skipped: 0,
            failed: 0,
            cursor: 0,
            last_path: None,
            reason: None,
            errors: Vec::new(),
            preview: Vec::new(),
            settings_hash: None,
            queue: Vec::new(),
        }
    }
}

impl GenreBackfillRun {
    pub fn can_resume(&self) -> bool {
        matches!(
            self.status,
            GenreBackfillStatus::Cancelled | GenreBackfillStatus::Interrupted
        ) && i64::from(self.cursor) < self.queue.len() as i64
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct GenreBackfillRunOut<'a> {
    run_id: &'a str,
    status: GenreBackfillStatus,
    scope: GenreBackfillScope,
    dry_run: bool,
    #[serde(serialize_with = "utc_option")]
    started_utc: &'a Option<DateTime<Utc>>,
    #[serde(serialize_with = "utc_option")]
    finished_utc: &'a Option<DateTime<Utc>>,
    total: i32,
    processed: i32,
    changed: i32,
    cleared: i32,
    skipped: i32,
    failed: i32,
    cursor: i32,
    last_path: &'a Option<String>,
    reason: &'a Option<String>,
    errors: &'a [String],
    preview: &'a [GenreBackfillChange],
    settings_hash: &'a Option<String>,
    queue: &'a [String],
    can_resume: bool,
}

fn utc_option<S: Serializer>(value: &&Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error> {
    octo_core::json::datetime::utc_option::serialize(value, serializer)
}

impl Serialize for GenreBackfillRun {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        GenreBackfillRunOut {
            run_id: &self.run_id,
            status: self.status,
            scope: self.scope,
            dry_run: self.dry_run,
            started_utc: &self.started_utc,
            finished_utc: &self.finished_utc,
            total: self.total,
            processed: self.processed,
            changed: self.changed,
            cleared: self.cleared,
            skipped: self.skipped,
            failed: self.failed,
            cursor: self.cursor,
            last_path: &self.last_path,
            reason: &self.reason,
            errors: &self.errors,
            preview: &self.preview,
            settings_hash: &self.settings_hash,
            queue: &self.queue,
            can_resume: self.can_resume(),
        }
        .serialize(serializer)
    }
}

/// The run's progress, on disk.
///
/// Persisted with the ExternalIdRegistry idiom (dirty bit, coalesced flush, atomic
/// temp-and-rename) for the same reason: flushing per file would turn a 1,900-file walk into
/// 1,900 extra writes, and a torn write would lose the cursor a resume depends on.
///
/// The C# store ran its own 10 s timer and flushed once more on `Dispose`; here that is
/// [`GenreBackfillStore::run_flusher`], registered as a worker by the app state.
pub struct GenreBackfillStore {
    path: Option<PathBuf>,
    run: Mutex<GenreBackfillRun>,
    dirty: AtomicBool,
}

impl GenreBackfillStore {
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

    /// Enough rows to review before applying, bounded so a whole-library preview
    /// cannot grow the state file without limit.
    pub const MAX_PREVIEW_ROWS: usize = 500;

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
    pub fn current(&self) -> GenreBackfillRun {
        self.run.lock().clone()
    }

    /// Reads the run without copying it (its queue can be thousands of paths).
    pub fn read<R>(&self, read: impl FnOnce(&GenreBackfillRun) -> R) -> R {
        read(&self.run.lock())
    }

    pub fn update(&self, mutate: impl FnOnce(&mut GenreBackfillRun)) {
        {
            let mut run = self.run.lock();
            mutate(&mut run);
            if run.errors.len() > Self::MAX_ERRORS {
                let excess = run.errors.len() - Self::MAX_ERRORS;
                run.errors.drain(..excess);
            }
            run.preview.truncate(Self::MAX_PREVIEW_ROWS);
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    pub fn replace(&self, run: GenreBackfillRun) {
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
            warn!("genre backfill state could not be written: {error}");
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

fn load(path: &Path) -> Option<GenreBackfillRun> {
    if !path.exists() {
        return None;
    }
    let read = state_file::read_text(path)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            serde_json::from_str::<Option<GenreBackfillRun>>(&text).map_err(|error| error.to_string())
        });
    let mut run = match read {
        Ok(run) => run?,
        Err(error) => {
            // Losing progress is a cold start, not a failure to boot.
            warn!("genre backfill state could not be read: {error}");
            return None;
        }
    };

    // A run that was going when the process stopped is Interrupted, never resumed
    // automatically: the restart may have been how the user stopped it.
    if run.status == GenreBackfillStatus::Running {
        run.status = GenreBackfillStatus::Interrupted;
        run.reason = Some("Octo restarted while this run was in progress.".to_string());
        info!(
            "genre backfill was interrupted at {}/{}; waiting for an explicit resume",
            run.cursor, run.total
        );
    }
    Some(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("genre-backfill.json");
        (dir, path)
    }

    fn fixture() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/genre-backfill.json"
        );
        std::fs::read_to_string(path).expect("the fixture is in the repo")
    }

    #[test]
    fn genre_backfill_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        let run: GenreBackfillRun = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(run.status, GenreBackfillStatus::Cancelled);
        assert!(run.can_resume());
        assert_eq!(run.preview[0].before, ["R&B/Soul", "Pop"]);
        assert_eq!(octo_core::json::to_string(&run), text.trim_end_matches('\n'));
    }

    /// The store loads the fixture and writes it back unchanged (it is Cancelled, so the load
    /// leaves it alone).
    #[test]
    fn the_store_writes_the_fixture_back_unchanged() {
        let (_dir, path) = temp_path();
        let text = fixture();
        std::fs::write(&path, &text).expect("copied");
        let store = GenreBackfillStore::new(Some(path.clone()));
        store.replace(store.current());
        assert_eq!(
            std::fs::read_to_string(&path).expect("written"),
            text.trim_end_matches('\n')
        );
    }

    /// GenreBackfillStoreTests.Run_SurvivesARestart.
    #[test]
    fn run_survives_a_restart() {
        let (_dir, path) = temp_path();
        {
            let first = GenreBackfillStore::new(Some(path.clone()));
            first.replace(GenreBackfillRun {
                run_id: "abc123".into(),
                status: GenreBackfillStatus::Completed,
                total: 10,
                cursor: 10,
                changed: 4,
                ..Default::default()
            });
        }

        let second = GenreBackfillStore::new(Some(path));
        assert_eq!(second.current().run_id, "abc123");
        assert_eq!(second.current().changed, 4);
    }

    /// GenreBackfillStoreTests.Load_RunningBecomesInterruptedAndDoesNotRestartItself: a run
    /// that was going when the process stopped must NOT auto-resume: the user may have
    /// restarted Octo specifically to stop a mass rewrite.
    #[test]
    fn load_running_becomes_interrupted_and_does_not_restart_itself() {
        let (_dir, path) = temp_path();
        {
            let first = GenreBackfillStore::new(Some(path.clone()));
            first.replace(GenreBackfillRun {
                run_id: "abc123".into(),
                status: GenreBackfillStatus::Running,
                total: 100,
                cursor: 42,
                queue: (0..100).map(|i| format!("/music/{i}.flac")).collect(),
                ..Default::default()
            });
        }

        let second = GenreBackfillStore::new(Some(path));
        let current = second.current();
        assert_eq!(current.status, GenreBackfillStatus::Interrupted);
        assert!(current.can_resume());
        assert_eq!(current.cursor, 42);
        assert_eq!(
            current.reason.as_deref(),
            Some("Octo restarted while this run was in progress.")
        );
    }

    /// GenreBackfillStoreTests.CanResume_IsFalseOnceTheQueueIsExhausted.
    #[test]
    fn can_resume_is_false_once_the_queue_is_exhausted() {
        let store = GenreBackfillStore::new(None);
        store.replace(GenreBackfillRun {
            status: GenreBackfillStatus::Cancelled,
            cursor: 3,
            queue: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        });

        assert!(!store.current().can_resume());
    }

    /// GenreBackfillStoreTests.Update_BoundsThePreviewAndTheErrorList: a whole-library
    /// preview would otherwise grow the state file without limit, and the state file is
    /// flushed repeatedly during a run.
    #[test]
    fn update_bounds_the_preview_and_the_error_list() {
        let store = GenreBackfillStore::new(None);
        store.update(|run| {
            for i in 0..GenreBackfillStore::MAX_PREVIEW_ROWS + 50 {
                run.preview.push(GenreBackfillChange::new(
                    format!("/music/{i}.flac"),
                    vec!["Music".into()],
                    vec!["Pop".into()],
                    "Write",
                    None,
                ));
            }
            for i in 0..100 {
                run.errors.push(format!("error {i}"));
            }
        });

        let current = store.current();
        assert_eq!(current.preview.len(), GenreBackfillStore::MAX_PREVIEW_ROWS);
        assert_eq!(current.errors.len(), 20);
        assert_eq!(current.errors.last().map(String::as_str), Some("error 99"));
    }

    /// GenreBackfillStoreTests.Load_UnreadableFile_StartsIdle: a state file that will not
    /// parse is a cold start, not a failure to boot.
    #[test]
    fn load_unreadable_file_starts_idle() {
        let (_dir, path) = temp_path();
        std::fs::write(&path, "{ not the shape we wrote").expect("written");
        let store = GenreBackfillStore::new(Some(path));
        assert_eq!(store.current().status, GenreBackfillStatus::Idle);
        assert!(store.current().dry_run);
    }

    /// An update is written by the flusher, not at once; cancelling it flushes what is left.
    #[tokio::test(start_paused = true)]
    async fn updates_are_flushed_on_the_timer_and_at_shutdown() {
        let (_dir, path) = temp_path();
        let store = std::sync::Arc::new(GenreBackfillStore::new(Some(path.clone())));
        let stopping = CancellationToken::new();
        let flusher = tokio::spawn({
            let store = store.clone();
            let stopping = stopping.clone();
            async move { store.run_flusher(stopping).await }
        });

        store.update(|run| run.run_id = "first".into());
        assert!(!path.exists());
        tokio::time::sleep(GenreBackfillStore::FLUSH_INTERVAL + Duration::from_millis(10)).await;
        assert!(
            std::fs::read_to_string(&path)
                .expect("flushed")
                .contains("\"first\"")
        );

        store.update(|run| run.run_id = "second".into());
        stopping.cancel();
        flusher.await.expect("the flusher ends");
        assert!(
            std::fs::read_to_string(&path)
                .expect("flushed")
                .contains("\"second\"")
        );
    }
}
