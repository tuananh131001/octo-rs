//! Port of `Services/Library/LibraryActionExecutor.cs`: only the outcome record.
//!
//! STUB(5-A): replaced when 5-A (library actions) lands with the executor.

use super::library_action_journal::LibraryActionState;

/// What happened to one library action request.
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
