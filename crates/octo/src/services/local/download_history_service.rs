//! Port of `Services/Local/DownloadHistoryService.cs`, the store behind `downloads-history.json`.

use std::path::{Path, PathBuf};

use octo_core::models::download::DownloadHistoryEntry;
use parking_lot::Mutex;
use tracing::warn;

use crate::services::state_file;

/// Persistent, bounded log of songs Octo has fetched. Written to a JSON file next
/// to the settings file so it survives restarts and container recreation (the
/// config dir is a host bind-mount). Newest entries first; capped so it can't grow
/// without limit. Best-effort — a write failure never breaks a download.
pub struct DownloadHistoryService {
    path: PathBuf,
    /// Loaded on first use.
    cache: Mutex<Option<Vec<DownloadHistoryEntry>>>,
}

impl DownloadHistoryService {
    pub const MAX_ENTRIES: usize = 500;

    pub fn new(path: impl AsRef<Path>) -> Self {
        DownloadHistoryService {
            path: path.as_ref().to_path_buf(),
            cache: Mutex::new(None),
        }
    }

    /// Append a fetched-song entry (newest first) and persist.
    pub fn record(&self, entry: DownloadHistoryEntry) {
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        list.insert(0, entry);
        list.truncate(Self::MAX_ENTRIES);
        self.save_locked(list);
    }

    /// The most recent entries, newest first. The C# default was 200.
    pub fn get_recent(&self, limit: i32) -> Vec<DownloadHistoryEntry> {
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        list.iter().take(limit.max(0) as usize).cloned().collect()
    }

    fn load_locked<'a>(
        &self,
        cache: &'a mut Option<Vec<DownloadHistoryEntry>>,
    ) -> &'a mut Vec<DownloadHistoryEntry> {
        cache.get_or_insert_with(|| {
            let loaded = (|| -> anyhow::Result<Vec<DownloadHistoryEntry>> {
                let Some(json) = state_file::read_text(&self.path)? else {
                    return Ok(Vec::new());
                };
                if json.trim().is_empty() {
                    return Ok(Vec::new());
                }
                Ok(serde_json::from_str::<Option<Vec<DownloadHistoryEntry>>>(&json)?.unwrap_or_default())
            })();
            loaded.unwrap_or_else(|e| {
                warn!("Download history load failed ({e}); starting fresh");
                Vec::new()
            })
        })
    }

    fn save_locked(&self, list: &[DownloadHistoryEntry]) {
        if let Err(e) = state_file::save_atomic(&self.path, &octo_core::json::to_string(list)) {
            warn!("Download history save failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/downloads-history.json"
    );

    fn entry(title: &str) -> DownloadHistoryEntry {
        DownloadHistoryEntry {
            artist: "A".into(),
            title: title.into(),
            downloaded_at: "2026-10-02T09:01:44.0807210Z".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_record_goes_first_and_the_rest_of_the_fixture_is_written_back_unchanged() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
        std::fs::write(&path, &text).expect("writes");

        let service = DownloadHistoryService::new(&path);
        let before = service.get_recent(200).len();
        service.record(entry("New"));

        let written = std::fs::read_to_string(&path).expect("reads");
        let expected_tail = &text.trim_end_matches('\n')[1..];
        assert!(
            written.ends_with(expected_tail),
            "the old entries keep their bytes"
        );
        assert!(written.starts_with(r#"[{"Artist":"A","Title":"New","#));
        assert_eq!(service.get_recent(200).len(), before + 1);
        assert_eq!(service.get_recent(1)[0].title, "New");
        assert!(service.get_recent(-5).is_empty());
    }

    #[test]
    fn the_log_is_capped_at_500_newest_first() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config").join("downloads-history.json");
        let service = DownloadHistoryService::new(&path);
        for i in 0..505 {
            service.record(entry(&i.to_string()));
        }
        let reopened = DownloadHistoryService::new(&path);
        let all = reopened.get_recent(1000);
        assert_eq!(all.len(), 500);
        assert_eq!(all[0].title, "504");
        assert_eq!(all[499].title, "5");
    }

    #[test]
    fn a_missing_blank_or_corrupt_file_starts_empty_and_is_overwritten() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        assert!(DownloadHistoryService::new(&path).get_recent(10).is_empty());
        std::fs::write(&path, "  \n").expect("writes");
        assert!(DownloadHistoryService::new(&path).get_recent(10).is_empty());
        std::fs::write(&path, "[{\"Artist\":").expect("writes");
        let service = DownloadHistoryService::new(&path);
        assert!(service.get_recent(10).is_empty());
        service.record(entry("x"));
        assert_eq!(DownloadHistoryService::new(&path).get_recent(10).len(), 1);
    }
}
