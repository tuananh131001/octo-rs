//! Port of `Services/Library/LibraryActionJournal.cs`: the state enum, the entry, and an
//! in-memory journal that only records and answers `IsNeverRequested`.
//!
//! STUB(5-A): replaced when 5-A (library actions) lands with the journal. The journal stub is
//! here for the Lidarr heart (4-E), which asks it whether a song was removed.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::settings::LibraryAction;
use parking_lot::Mutex;

use super::PathSource;

/// What happened to one library action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LibraryActionState {
    /// Written BEFORE the file is touched. A crash leaves this behind, and startup
    /// reconciles it against the filesystem rather than blindly re-running.
    #[default]
    Pending,
    Applied,
    Failed,

    /// The id could not be resolved to a file. Never a success, never a delete.
    Unresolved,
    Skipped,

    /// A rehearsal: everything ran except touching the file. Its own state rather than Failed,
    /// because nothing failed, and a log line saying otherwise about a working dry run is the
    /// kind of thing that makes someone turn rehearsal mode off to "fix" it.
    Rehearsed,
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

/// One library action, as the journal records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryActionEntry {
    pub key: String,
    pub action: LibraryAction,
    pub navidrome_id: String,
    pub username: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_path: Option<String>,
    pub quarantine_path: Option<String>,
    pub resolution: Option<PathSource>,
    pub state: LibraryActionState,
    pub detail: Option<String>,
    pub dry_run: bool,
    pub at_utc: DateTime<Utc>,
    /// Whether Navidrome kept a replaced song as the same song (W8); None until checked.
    pub history_kept: Option<bool>,
    /// Where a replacement moved in, recorded the moment it did. A restart after that
    /// finds the swap done instead of putting the original back beside it.
    pub revealed_path: Option<String>,
}

/// STUB(5-A): the write-ahead ledger for library actions, in memory and without its bound,
/// its file or its reconciling. Only `Record` and `IsNeverRequested` are here.
#[derive(Default)]
pub struct LibraryActionJournal {
    by_key: Mutex<HashMap<String, LibraryActionEntry>>,
}

impl LibraryActionJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, entry: LibraryActionEntry) {
        self.by_key.lock().insert(entry.key.clone(), entry);
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
        self.by_key.lock().values().any(|entry| {
            entry.action == LibraryAction::Delete
                && entry.state == LibraryActionState::Applied
                && !entry.dry_run
                && SongIdentity::match_key(&entry.artist, &entry.title) == wanted
        })
    }
}
