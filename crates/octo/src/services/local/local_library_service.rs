//! STUB(4-D): replaced when 4-D lands with the port of `Services/Local/LocalLibraryService.cs`.
//!
//! Until then the service knows no downloads, so `find_mapping_by_tags` never matches and
//! `NavidromeSongPathResolver`'s last-resort leg finds nothing, which fails safe.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::ILocalLibraryService;

/// Represents the mapping between an external song and its local file
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalSongMapping {
    pub external_provider: String,
    pub external_id: String,
    pub local_path: String,
    pub local_subsonic_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub downloaded_at: DateTime<Utc>,
    pub source_peer: Option<String>,
    pub source_file: Option<String>,
    pub music_brainz_recording_id: Option<String>,
    pub transcoded_from: Option<String>,
}

#[derive(Debug, Default)]
pub struct LocalLibraryService;

impl LocalLibraryService {
    pub fn new() -> Self {
        LocalLibraryService
    }
}

#[async_trait]
impl ILocalLibraryService for LocalLibraryService {
    async fn find_mapping_by_tags(
        &self,
        _artist: Option<&str>,
        _title: Option<&str>,
        _album: Option<&str>,
    ) -> Option<LocalSongMapping> {
        None
    }

    async fn trigger_library_scan(&self, _force: bool) -> bool {
        false
    }
}
