//! Port of `Services/Library/LibraryActionJournal.cs`: only the state enum.
//!
//! STUB(5-A): replaced when 5-A (library actions) lands with the journal.

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
