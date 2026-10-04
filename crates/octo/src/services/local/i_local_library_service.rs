//! Port of `Services/Local/ILocalLibraryService.cs`.

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;

use super::LocalSongMapping;

/// What [`ILocalLibraryService::parse_song_id`] answers: whether the id is external, its
/// provider and its external id.
pub type ParsedSongId = (bool, Option<String>, Option<String>);

/// What [`ILocalLibraryService::parse_external_id`] answers: whether the id is external, its
/// provider, its type ("song", "album" or "artist") and its external id.
pub type ParsedExternalId = (bool, Option<String>, Option<String>, Option<String>);

/// Interface for local music library management
#[async_trait]
pub trait ILocalLibraryService: Send + Sync {
    /// Checks if an external song already exists locally
    async fn get_local_path_for_external_song(
        &self,
        external_provider: &str,
        external_id: &str,
    ) -> Option<String>;

    /// Registers a downloaded song in the local library. `Err` where the C# threw: the
    /// mappings file could not be written.
    async fn register_downloaded_song(&self, song: &Song, local_path: &str) -> anyhow::Result<()>;

    /// Gets the mapping between external ID and local ID
    async fn get_local_id_for_external_song(
        &self,
        external_provider: &str,
        external_id: &str,
    ) -> Option<String>;

    /// Parses a song ID to determine if it is external or local
    fn parse_song_id(&self, song_id: &str) -> ParsedSongId;

    /// Parses an external ID to extract the provider, type and ID
    /// Format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259, ext-deezer-album-96126, ext-deezer-song-12345)
    /// Also supports legacy format: ext-{provider}-{id} (assumes song type)
    fn parse_external_id(&self, id: &str) -> ParsedExternalId;

    /// Every download Octo has a record of. The genre backfill uses this to scope a run to
    /// files Octo itself created, which is the only scope where rewriting a tag is rewriting
    /// our own output rather than someone's hand-curated rip.
    async fn get_mappings(&self) -> Vec<LocalSongMapping>;

    /// The mapping whose tags match, or `None` when there is no match OR more than one.
    /// Ambiguity is a failure, not a coin flip: this feeds a delete, and picking arbitrarily
    /// between two candidates is how you delete the wrong one.
    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping>;

    /// Drop the mapping for a path Octo no longer owns, so a re-acquire is not
    /// short-circuited by a file that has just been moved out of the library. `Err` where the
    /// C# threw: the mappings file could not be written.
    async fn forget_mapping(&self, local_path: &str) -> anyhow::Result<bool>;

    /// Triggers a Subsonic library scan. `force` bypasses the debounce. Needed when a caller
    /// must guarantee the scan actually runs, e.g. after each track of an album download so the
    /// album fills in progressively and the final tracks are never left stranded by a
    /// swallowed trigger. (The C# default was `false`.)
    async fn trigger_library_scan(&self, force: bool) -> bool;

    /// Gets the current scan status
    async fn get_scan_status(&self) -> Option<ScanStatus>;
}
