//! A hand-written `Mock<ILocalLibraryService>` for the tests of the services that take one.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;
use parking_lot::Mutex;

use super::{ILocalLibraryService, LocalSongMapping, ParsedExternalId, ParsedSongId};

/// Knows nothing unless told: `find_mapping_by_tags` answers the mappings set up for exactly
/// those tags (as `Setup(l => l.FindMappingByTagsAsync("A", "Song", null))` did), scans are
/// counted and succeed, and every id is local.
#[derive(Default)]
pub(crate) struct FakeLocalLibrary {
    pub by_tags: Mutex<Vec<(String, String, Option<String>, LocalSongMapping)>>,
    pub scans: AtomicUsize,
}

impl FakeLocalLibrary {
    pub fn with_mapping(artist: &str, title: &str, album: Option<&str>, mapping: LocalSongMapping) -> Self {
        let fake = FakeLocalLibrary::default();
        fake.by_tags
            .lock()
            .push((artist.into(), title.into(), album.map(str::to_string), mapping));
        fake
    }

    pub fn scans(&self) -> usize {
        self.scans.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ILocalLibraryService for FakeLocalLibrary {
    async fn get_local_path_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn register_downloaded_song(&self, _: &Song, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    fn parse_song_id(&self, _: &str) -> ParsedSongId {
        (false, None, None)
    }

    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.by_tags.lock().iter().map(|(.., m)| m.clone()).collect()
    }

    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping> {
        self.by_tags
            .lock()
            .iter()
            .find(|(a, t, al, _)| {
                Some(a.as_str()) == artist && Some(t.as_str()) == title && al.as_deref() == album
            })
            .map(|(.., m)| m.clone())
    }

    async fn forget_mapping(&self, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn trigger_library_scan(&self, _force: bool) -> bool {
        self.scans.fetch_add(1, Ordering::SeqCst);
        true
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        None
    }
}
