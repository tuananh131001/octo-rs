//! Port of `Services/Common/SoulseekHoldStore.cs`, the store behind `soulseek-holds.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};
use tracing::warn;

use crate::services::state_file;

/// Appended, never inserted: the file stores these as numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum HeldKind {
    #[default]
    Track = 0,
    Album = 1,
}

impl HeldKind {
    /// The enum name, as `{Kind}` interpolated it.
    pub fn name(self) -> &'static str {
        match self {
            HeldKind::Track => "Track",
            HeldKind::Album => "Album",
        }
    }
}

/// A heart waiting for Soulseek. HeldSinceUtc survives a restart, so the wait is the
/// setting's length in total, not that long again after every restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeldAcquisition {
    pub kind: HeldKind,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub provider: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub external_id: String,
    pub requested_by: Option<String>,
    #[serde(with = "datetime::utc")]
    pub held_since_utc: DateTime<Utc>,
}

impl Default for HeldAcquisition {
    fn default() -> Self {
        HeldAcquisition {
            kind: HeldKind::Track,
            provider: String::new(),
            external_id: String::new(),
            requested_by: None,
            held_since_utc: datetime::min_value(),
        }
    }
}

impl HeldAcquisition {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.kind.name(), self.provider, self.external_id)
    }
}

/// By hand, because System.Text.Json also wrote the computed `Key` (ignored on read).
impl Serialize for HeldAcquisition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("HeldAcquisition", 6)?;
        s.serialize_field("Kind", &self.kind)?;
        s.serialize_field("Provider", &self.provider)?;
        s.serialize_field("ExternalId", &self.external_id)?;
        s.serialize_field("RequestedBy", &self.requested_by)?;
        s.serialize_field("HeldSinceUtc", &datetime::format_utc(&self.held_since_utc))?;
        s.serialize_field("Key", &self.key())?;
        s.end()
    }
}

/// The hearts waiting out a Soulseek outage, on disk. Without it a restart during a six hour
/// wait would drop every one of them without a word, where before the wait existed they at
/// least landed as MP3s. Written on every change, inside the lock: there are a handful, and
/// only during an outage, and two writes outside it could land in the wrong order.
pub struct SoulseekHoldStore {
    path: Option<PathBuf>,
    held: Mutex<DotnetDictionary<HeldAcquisition>>,
}

impl SoulseekHoldStore {
    /// A store kept at `path` (loaded now), or in memory only when it is absent or blank.
    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = path
            .map(|p| p.as_ref().to_path_buf())
            .filter(|p| !p.as_os_str().to_string_lossy().trim().is_empty());
        let mut held = DotnetDictionary::default();
        if let Some(path) = &path {
            let loaded = (|| -> anyhow::Result<Vec<HeldAcquisition>> {
                let Some(text) = state_file::read_text(path)? else {
                    return Ok(Vec::new());
                };
                Ok(serde_json::from_str::<Option<Vec<HeldAcquisition>>>(&text)?.unwrap_or_default())
            })();
            match loaded {
                Ok(list) => {
                    for h in list {
                        held.set(h.key(), h);
                    }
                }
                Err(e) => warn!("held downloads could not be read: {e}"),
            }
        }
        SoulseekHoldStore {
            path,
            held: Mutex::new(held),
        }
    }

    /// Every hold, oldest first.
    pub fn snapshot(&self) -> Vec<HeldAcquisition> {
        let mut list: Vec<HeldAcquisition> = self.held.lock().values().cloned().collect();
        // OrderBy is stable.
        list.sort_by_key(|h| h.held_since_utc);
        list
    }

    /// Records a hold. One already there keeps its first start time.
    pub fn hold(&self, held: HeldAcquisition) -> HeldAcquisition {
        let mut map = self.held.lock();
        let key = held.key();
        if let Some(existing) = map.get(&key) {
            return existing.clone();
        }
        map.set(key, held.clone());
        self.save(&map);
        held
    }

    pub fn release(&self, key: &str) {
        let mut map = self.held.lock();
        if map.remove(key) {
            self.save(&map);
        }
    }

    // Called with the lock held.
    fn save(&self, map: &DotnetDictionary<HeldAcquisition>) {
        let Some(path) = &self.path else { return };
        let list: Vec<&HeldAcquisition> = map.values().collect();
        if let Err(e) = state_file::save_atomic(path, &octo_core::json::to_string(&list)) {
            warn!("held downloads could not be written: {e}");
        }
    }
}

/// A `Dictionary<string, T>`'s enumeration order: insertion order, except that an entry added
/// after a removal takes the most recently freed slot. The file is written in this order.
#[derive(Debug)]
struct DotnetDictionary<T> {
    slots: Vec<Option<(String, T)>>,
    index: HashMap<String, usize>,
    /// Freed slots, the most recently freed last.
    free: Vec<usize>,
}

impl<T> Default for DotnetDictionary<T> {
    fn default() -> Self {
        DotnetDictionary {
            slots: Vec::new(),
            index: HashMap::new(),
            free: Vec::new(),
        }
    }
}

impl<T> DotnetDictionary<T> {
    fn get(&self, key: &str) -> Option<&T> {
        self.index
            .get(key)
            .and_then(|&i| self.slots[i].as_ref().map(|(_, v)| v))
    }

    /// `dict[key] = value`: replaces in place, or adds.
    fn set(&mut self, key: String, value: T) {
        if let Some(&i) = self.index.get(&key) {
            self.slots[i] = Some((key, value));
            return;
        }
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.index.insert(key.clone(), i);
        self.slots[i] = Some((key, value));
    }

    fn remove(&mut self, key: &str) -> bool {
        let Some(i) = self.index.remove(key) else {
            return false;
        };
        self.slots[i] = None;
        self.free.push(i);
        true
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|s| s.as_ref().map(|(_, v)| v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/soulseek-holds.json"
    );

    fn held(kind: HeldKind, id: &str, minute: u32) -> HeldAcquisition {
        HeldAcquisition {
            kind,
            provider: "soulseek".into(),
            external_id: id.into(),
            requested_by: Some("brandon".into()),
            held_since_utc: Utc.with_ymd_and_hms(2026, 10, 3, 12, minute, 0).unwrap(),
        }
    }

    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
        let list: Vec<HeldAcquisition> = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(list[1].key(), "Album:soulseek:Ab9Cd8Ef7Gh6Ij5Kl4Mn3o");
        assert_eq!(octo_core::json::to_string(&list), text.trim_end_matches('\n'));
    }

    #[test]
    fn holds_survive_a_restart_and_a_second_hold_keeps_the_first_start() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config").join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        let first = store.hold(held(HeldKind::Track, "a", 5));
        let again = store.hold(held(HeldKind::Track, "a", 9));
        assert_eq!(again, first);
        store.hold(held(HeldKind::Album, "b", 1));

        let reopened = SoulseekHoldStore::new(Some(&path));
        let keys: Vec<String> = reopened.snapshot().iter().map(HeldAcquisition::key).collect();
        assert_eq!(keys, vec!["Album:soulseek:b", "Track:soulseek:a"], "oldest first");

        reopened.release("Album:soulseek:b");
        reopened.release("nothing");
        assert_eq!(SoulseekHoldStore::new(Some(&path)).snapshot().len(), 1);
    }

    #[test]
    fn the_file_keeps_dictionary_order_with_freed_slots_reused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        store.hold(held(HeldKind::Track, "a", 1));
        store.hold(held(HeldKind::Track, "b", 2));
        store.hold(held(HeldKind::Track, "c", 3));
        store.release("Track:soulseek:a");
        store.hold(held(HeldKind::Track, "d", 4));
        let written: Vec<HeldAcquisition> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("reads")).expect("parses");
        let ids: Vec<&str> = written.iter().map(|h| h.external_id.as_str()).collect();
        assert_eq!(ids, vec!["d", "b", "c"]);
    }

    #[test]
    fn an_unreadable_file_or_no_path_means_no_holds() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        std::fs::write(&path, "[{").expect("writes");
        assert!(SoulseekHoldStore::new(Some(&path)).snapshot().is_empty());
        let memory = SoulseekHoldStore::new(None::<PathBuf>);
        memory.hold(held(HeldKind::Track, "a", 1));
        assert_eq!(memory.snapshot().len(), 1);
    }
}
