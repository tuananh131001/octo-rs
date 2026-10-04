//! STUB(4-D): replaced when 4-D lands with the full port of
//! `Services/Local/ILocalLibraryService.cs`. Only the member `NavidromeSongPathResolver` (3-E)
//! calls is here.

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
}
