//! `Services/Local/ILocalLibraryService.cs`, as far as the lyrics writer needs it.
// STUB(4-D): replaced when 4-D (LocalLibraryService) lands. Only the one member the lyrics
// writer calls is here; 4-D adds the rest of the interface and the implementation.

use async_trait::async_trait;

/// Interface for local music library management.
#[async_trait]
pub trait ILocalLibraryService: Send + Sync {
    /// Triggers a Subsonic library scan. `force` bypasses the debounce. Needed when a caller
    /// must guarantee the scan actually runs, e.g. after each track of an album download so the
    /// album fills in progressively and the final tracks are never left stranded by a
    /// swallowed trigger.
    async fn trigger_library_scan(&self, force: bool) -> bool;
}
