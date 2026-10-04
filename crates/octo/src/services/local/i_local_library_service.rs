//! `Services/Local/ILocalLibraryService.cs`, as far as the services ported so far need it.
// STUB(4-D): replaced when 4-D (LocalLibraryService) lands. Only the members the song path
// resolver (3-E) and the lyrics writer (3-C) call are here; 4-D adds the rest of the interface.

use async_trait::async_trait;

use super::LocalSongMapping;

/// Interface for local music library management
#[async_trait]
pub trait ILocalLibraryService: Send + Sync {
    /// The mapping whose tags match, or `None` when there is no match OR more than one.
    /// Ambiguity is a failure, not a coin flip: this feeds a delete, and picking arbitrarily
    /// between two candidates is how you delete the wrong one.
    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping>;

    /// Triggers a Subsonic library scan. `force` bypasses the debounce. Needed when a caller
    /// must guarantee the scan actually runs, e.g. after each track of an album download so the
    /// album fills in progressively and the final tracks are never left stranded by a
    /// swallowed trigger.
    async fn trigger_library_scan(&self, force: bool) -> bool;
}
