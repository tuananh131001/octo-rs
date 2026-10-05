//! Port of `Services/Library/LibraryActionJournal.cs`: the write-ahead ledger for library
//! actions, `<config>/library-actions.json` (state-files.md §4.12).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::json::datetime;
use octo_core::settings::LibraryAction;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::PathSource;
use crate::services::state_file;

/// What happened to one library action. Written as its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum LibraryActionState {
    /// Written BEFORE the file is touched. A crash leaves this behind, and startup
    /// reconciles it against the filesystem rather than blindly re-running.
    #[default]
    Pending = 0,
    Applied = 1,
    Failed = 2,

    /// The id could not be resolved to a file. Never a success, never a delete.
    Unresolved = 3,
    Skipped = 4,

    /// A rehearsal: everything ran except touching the file. Its own state rather than Failed,
    /// because nothing failed, and a log line saying otherwise about a working dry run is the
    /// kind of thing that makes someone turn rehearsal mode off to "fix" it.
    Rehearsed = 5,
}

impl LibraryActionState {
    /// The C# member name.
    pub fn name(self) -> &'static str {
        match self {
            LibraryActionState::Pending => "Pending",
            LibraryActionState::Applied => "Applied",
            LibraryActionState::Failed => "Failed",
            LibraryActionState::Unresolved => "Unresolved",
            LibraryActionState::Skipped => "Skipped",
            LibraryActionState::Rehearsed => "Rehearsed",
        }
    }
}

/// `action.ToString()`: the C# member name of a library action.
pub fn action_name(action: LibraryAction) -> &'static str {
    match action {
        LibraryAction::Delete => "Delete",
        LibraryAction::WrongSong => "WrongSong",
        LibraryAction::WrongVersion => "WrongVersion",
        LibraryAction::BetterQuality => "BetterQuality",
        LibraryAction::Keep => "Keep",
    }
}

/// One library action, as the journal records it. The fields in the C# record's order, then
/// the two body properties, which is the order System.Text.Json wrote them in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryActionEntry {
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub key: String,
    #[serde(default, with = "action_number")]
    pub action: LibraryAction,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub navidrome_id: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub username: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub title: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub artist: String,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub album: String,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub quarantine_path: Option<String>,
    #[serde(default, with = "path_source_number")]
    pub resolution: Option<PathSource>,
    #[serde(default)]
    pub state: LibraryActionState,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub at_utc: DateTime<Utc>,
    /// Whether Navidrome kept a replaced song as the same song (W8); None until checked.
    #[serde(default)]
    pub history_kept: Option<bool>,
    /// Where a replacement moved in, recorded the moment it did. A restart after that
    /// finds the swap done instead of putting the original back beside it.
    #[serde(default)]
    pub revealed_path: Option<String>,
}

/// `LibraryAction` as its number, which is how the journal stores it.
mod action_number {
    use octo_core::settings::LibraryAction;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &LibraryAction, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i32(value.as_index())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<LibraryAction, D::Error> {
        let number = i32::deserialize(d)?;
        LibraryAction::from_index(number)
            .ok_or_else(|| D::Error::custom(format!("no library action is numbered {number}")))
    }
}

/// `PathSource?` as its number, or null.
mod path_source_number {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use super::PathSource;

    const ALL: [PathSource; 4] = [
        PathSource::NativeApi,
        PathSource::SubsonicGetSong,
        PathSource::LocalMappings,
        PathSource::None,
    ];

    pub fn serialize<S: Serializer>(value: &Option<PathSource>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(source) => s.serialize_i32(ALL.iter().position(|s| s == source).unwrap_or(0) as i32),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathSource>, D::Error> {
        match Option::<i32>::deserialize(d)? {
            None => Ok(None),
            Some(number) => usize::try_from(number)
                .ok()
                .and_then(|i| ALL.get(i).copied())
                .map(Some)
                .ok_or_else(|| D::Error::custom(format!("no path source is numbered {number}"))),
        }
    }
}

/// The entries by key, and the keys oldest first.
#[derive(Default)]
struct Entries {
    by_key: HashMap<String, LibraryActionEntry>,
    order: VecDeque<String>,
}

impl Entries {
    fn ordered(&self) -> impl DoubleEndedIterator<Item = &LibraryActionEntry> {
        self.order.iter().filter_map(|key| self.by_key.get(key))
    }
}

/// Write-ahead ledger for library actions. Two jobs, and the order of operations is the point.
///
/// 1. Idempotency. The playlist and the file are two systems with no transaction across them.
///    Quarantine a file, fail to remove the track from the playlist, and the next sweep sees
///    the same track. Without a record of "already applied", a Wrong version would re-download
///    on every poll forever. So the entry is written Pending BEFORE the file is touched and
///    completed after; a crash in between leaves a Pending entry that startup RECONCILES.
///
/// 2. Audit. Every file this feature moved, who asked, when, from where to where, and whether
///    it was a dry run. That is what makes a delete reversible in practice rather than in
///    principle, and logs scroll away while "which files did this touch" has to be answerable
///    a week later.
///
/// The C# flushed from a 10 s `Timer` and from `Dispose`; here [`LibraryActionJournal::run_flusher`]
/// is the timer (a worker, which flushes once more on shutdown) and `Drop` is the `Dispose`.
pub struct LibraryActionJournal {
    path: Option<PathBuf>,
    entries: Mutex<Entries>,
    reconcile_lock: Mutex<()>,
    flush_lock: Mutex<()>,
    dirty: AtomicBool,
}

impl Default for LibraryActionJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryActionJournal {
    const MAX_ENTRIES: usize = 2000;
    const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

    /// A journal in memory only (`new LibraryActionJournal()`).
    pub fn new() -> Self {
        Self::with_path(None)
    }

    /// A journal kept in `path`, loaded now (`new LibraryActionJournal(path)`). None, or a blank
    /// path, keeps it in memory.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let path = path.filter(|p| !dotnet::is_blank(&p.to_string_lossy()));
        let journal = LibraryActionJournal {
            path,
            entries: Mutex::new(Entries::default()),
            reconcile_lock: Mutex::new(()),
            flush_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
        };
        if let Some(path) = journal.path.clone() {
            journal.load(&path);
        }
        journal
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Identity of an action.
    ///
    /// Fingerprinted on the file's size and modified time rather than the id alone, because
    /// after a Better quality upgrade the SAME Navidrome id points at a NEW file and the user
    /// is entitled to ask for a better copy of that one too.
    pub fn make_key(action: LibraryAction, navidrome_id: &str, fingerprint: &str) -> String {
        format!("{}|{navidrome_id}|{fingerprint}", action_name(action))
    }

    /// `{size}:{lastWriteUtc.Ticks}`.
    pub fn fingerprint(size_bytes: i64, last_write_ticks: i64) -> String {
        format!("{size_bytes}:{last_write_ticks}")
    }

    /// True when this exact action on this exact file content already reached a terminal state.
    ///
    /// A DRY RUN never counts, and that exclusion is load-bearing rather than tidy: a rehearsal
    /// records an entry under the same key, so without this a rehearsal would permanently
    /// suppress the real action for that file. Observed in exactly that form on the first live
    /// run, where the real sweep answered Skipped and quietly consumed the request.
    pub fn already_applied(&self, action: LibraryAction, navidrome_id: &str, fingerprint: &str) -> bool {
        self.entries
            .lock()
            .by_key
            .get(&Self::make_key(action, navidrome_id, fingerprint))
            .is_some_and(|entry| {
                !entry.dry_run
                    && matches!(
                        entry.state,
                        LibraryActionState::Applied | LibraryActionState::Skipped
                    )
            })
    }

    pub fn record(&self, entry: LibraryActionEntry) {
        {
            let mut entries = self.entries.lock();
            if !entries.by_key.contains_key(&entry.key) {
                entries.order.push_back(entry.key.clone());
            }
            entries.by_key.insert(entry.key.clone(), entry);
            Self::trim(&mut entries);
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Moves an entry to `state`. Each `None` keeps what the entry had; the time is now.
    pub fn complete(
        &self,
        key: &str,
        state: LibraryActionState,
        detail: Option<&str>,
        quarantine_path: Option<&str>,
        history_kept: Option<bool>,
        revealed_path: Option<&str>,
    ) {
        let Some(existing) = self.entries.lock().by_key.get(key).cloned() else {
            return;
        };
        self.record(LibraryActionEntry {
            state,
            detail: detail.map(str::to_string).or(existing.detail.clone()),
            quarantine_path: quarantine_path
                .map(str::to_string)
                .or(existing.quarantine_path.clone()),
            history_kept: history_kept.or(existing.history_kept),
            revealed_path: revealed_path
                .map(str::to_string)
                .or(existing.revealed_path.clone()),
            at_utc: Utc::now(),
            ..existing
        });
    }

    /// Has the user said they do not want this track back?
    ///
    /// A Delete means exactly that, so a later re-acquire of the same artist and title is
    /// refused. Read off the journal rather than a second store: it already records artist and
    /// title per entry and is bounded, so this is a scan over a couple of thousand rows in
    /// memory.
    pub fn is_never_requested(&self, artist: Option<&str>, title: Option<&str>) -> bool {
        let (Some(artist), Some(title)) = (artist, title) else {
            return false;
        };
        if dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return false;
        }

        // However the request writes it ("Drake - Too Good (feat. Rihanna)" for a deleted "Drake
        // feat. Rihanna - Too Good"), but a live take of a deleted song is still its own song.
        let wanted = SongIdentity::match_key(artist, title);
        self.entries.lock().by_key.values().any(|entry| {
            entry.action == LibraryAction::Delete
                && entry.state == LibraryActionState::Applied
                && !entry.dry_run
                && SongIdentity::match_key(&entry.artist, &entry.title) == wanted
        })
    }

    pub fn pending(&self) -> Vec<LibraryActionEntry> {
        self.entries
            .lock()
            .ordered()
            .filter(|entry| entry.state == LibraryActionState::Pending)
            .cloned()
            .collect()
    }

    /// Newest first, at most `limit` (the C# default was 200).
    pub fn recent(&self, limit: usize) -> Vec<LibraryActionEntry> {
        self.entries.lock().ordered().rev().take(limit).cloned().collect()
    }

    /// Decide what a Pending entry actually means, by looking at the filesystem rather than
    /// guessing. A half-written action must never re-delete and never re-download.
    ///
    /// The executor records the quarantine path on the Pending entry as soon as the move lands,
    /// before a replacement is fetched, so a crash during that minutes-long fetch is recognisable
    /// here. `restore_original` puts a quarantined file back; a replacement that never finished
    /// should not leave the user without the track. Runs under a lock because the playlist
    /// worker and the executor can both reconcile at startup, and a second restore of the same
    /// entry would overwrite the first's result with a wrong one.
    pub fn reconcile(&self, restore_original: Option<&dyn Fn(&str) -> bool>) -> usize {
        let _reconciling = self.reconcile_lock.lock();
        let mut reconciled = 0;
        for entry in self.pending() {
            let quarantine = entry
                .quarantine_path
                .as_deref()
                .filter(|recorded| !recorded.is_empty() && file_exists(recorded));
            let source_present = entry
                .source_path
                .as_deref()
                .is_some_and(|source| !source.is_empty() && file_exists(source));
            let (state, detail) = Self::resolve(&entry, quarantine, source_present, restore_original);
            self.complete(&entry.key, state, Some(&detail), None, None, None);
            reconciled += 1;
        }

        if reconciled > 0 {
            self.flush();
            info!("Library actions reconciled {reconciled} interrupted entr(ies)");
        }
        reconciled
    }

    fn resolve(
        entry: &LibraryActionEntry,
        quarantine: Option<&str>,
        source_present: bool,
        restore_original: Option<&dyn Fn(&str) -> bool>,
    ) -> (LibraryActionState, String) {
        // The replacement had already taken the original's place: the action is done, and the
        // original stays in quarantine until the retention sweep.
        if entry.action != LibraryAction::Delete
            && let Some(revealed) = entry.revealed_path.as_deref().filter(|r| !r.is_empty())
            && file_exists(revealed)
        {
            return (
                LibraryActionState::Applied,
                match quarantine {
                    None => format!("Reconciled after a restart: the replacement is in place at {revealed}."),
                    Some(quarantine) => format!(
                        "Reconciled after a restart: the replacement is in place at {revealed} and the original is in quarantine at {quarantine}."
                    ),
                },
            );
        }

        let Some(quarantine) = quarantine else {
            return if source_present {
                // The file is still where it was, so nothing happened. Failed rather than left
                // Pending, so it is not treated as in flight forever.
                (
                    LibraryActionState::Failed,
                    "Octo stopped before this action was applied; nothing was changed.".to_string(),
                )
            } else {
                (
                    LibraryActionState::Failed,
                    format!(
                        "Octo stopped partway through. The file is no longer at {}; check the quarantine folder.",
                        entry.source_path.as_deref().unwrap_or("")
                    ),
                )
            };
        };

        if entry.action == LibraryAction::Delete {
            return if source_present {
                (
                    LibraryActionState::Failed,
                    format!(
                        "Octo stopped partway. The file is back at its path and a copy is in quarantine at {quarantine}."
                    ),
                )
            } else {
                // The move landed and only the bookkeeping after it was lost: the removal is done.
                (
                    LibraryActionState::Applied,
                    "Reconciled after a restart.".to_string(),
                )
            };
        }

        // A replacement was being fetched when Octo stopped.
        if source_present {
            // Something is at the original path again (a replacement may have landed). Putting
            // the original back would overwrite it, so leave both for the user to compare.
            return (
                LibraryActionState::Failed,
                format!(
                    "Octo stopped while replacing this track. A file is at the original path and the original is also in quarantine at {quarantine}; compare them before removing either."
                ),
            );
        }

        if restore_original.is_some_and(|restore| restore(quarantine)) {
            (
                LibraryActionState::Failed,
                "Octo stopped while replacing this track. The original was put back.".to_string(),
            )
        } else {
            (
                LibraryActionState::Failed,
                format!(
                    "Octo stopped while replacing this track. The original is in quarantine at {quarantine}."
                ),
            )
        }
    }

    fn trim(entries: &mut Entries) {
        while entries.order.len() > Self::MAX_ENTRIES {
            if let Some(oldest) = entries.order.pop_front() {
                entries.by_key.remove(&oldest);
            }
        }
    }

    fn load(&self, path: &Path) {
        let attempt: anyhow::Result<()> = (|| {
            let Some(text) = state_file::read_text(path)? else {
                return Ok(());
            };
            let Some(loaded) = serde_json::from_str::<Option<Vec<LibraryActionEntry>>>(&text)? else {
                return Ok(());
            };
            let count = {
                let mut entries = self.entries.lock();
                for entry in loaded {
                    if entry.key.is_empty() {
                        continue;
                    }
                    if !entries.by_key.contains_key(&entry.key) {
                        entries.order.push_back(entry.key.clone());
                    }
                    entries.by_key.insert(entry.key.clone(), entry);
                }
                Self::trim(&mut entries);
                entries.by_key.len()
            };
            if count > 0 {
                info!("library action journal restored {count} entries");
            }
            Ok(())
        })();
        if let Err(e) = attempt {
            warn!("library action journal could not be read: {e}");
        }
    }

    /// Write the journal to disk now rather than on the next timer tick. The executor calls this
    /// around moving a file, because an entry that only exists in memory when Octo stops cannot
    /// be reconciled. Serialised, since the timer can flush at the same moment.
    ///
    /// False only when there was something to write and it could not be written.
    pub fn flush(&self) -> bool {
        let Some(path) = self.path.as_deref() else {
            return true;
        };
        let _flushing = self.flush_lock.lock();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return true;
        }
        let json = {
            let entries = self.entries.lock();
            octo_core::json::to_string(&entries.ordered().collect::<Vec<_>>())
        };
        match state_file::save_atomic(path, &json) {
            Ok(()) => true,
            Err(e) => {
                self.dirty.store(true, Ordering::SeqCst);
                warn!("library action journal could not be written: {e}");
                false
            }
        }
    }

    /// The 10-second flush timer, and the flush `Dispose` did on shutdown, as a worker.
    pub async fn run_flusher(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let journal = self.clone();
        state_file::flush_every(
            Self::FLUSH_INTERVAL,
            token,
            Arc::new(move || {
                journal.flush();
            }),
        )
        .await
    }
}

impl Drop for LibraryActionJournal {
    /// `Dispose`: the last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
#[path = "library_action_journal_tests.rs"]
mod tests;
