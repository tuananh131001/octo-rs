//! Port of `LyricsChoiceStore` from `Services/Lyrics/LyricsChoices.cs`: the pins on disk
//! (`lyrics-choices.json`). The pin itself is `octo_core::lyrics::LyricsPin`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::is_blank;
use octo_core::lyrics::LyricsPin;
use parking_lot::Mutex;
use tracing::warn;

use crate::services::state_file;

/// The pins by song id, enumerated as .NET's `Dictionary<string, LyricsPin>` enumerated them:
/// in insertion order, except that a removed entry's slot is reused by the next one added
/// (the most recently freed first). The file is written in that order.
#[derive(Default)]
struct Pins {
    slots: Vec<Option<LyricsPin>>,
    free: Vec<usize>,
    index: HashMap<String, usize>,
}

impl Pins {
    fn get(&self, song_id: &str) -> Option<&LyricsPin> {
        self.index
            .get(song_id)
            .and_then(|&slot| self.slots[slot].as_ref())
    }

    /// `_pins[pin.SongId] = pin`: replaced in place, or added.
    fn set(&mut self, pin: LyricsPin) {
        if let Some(&slot) = self.index.get(&pin.song_id) {
            self.slots[slot] = Some(pin);
            return;
        }
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.index.insert(pin.song_id.clone(), slot);
        self.slots[slot] = Some(pin);
    }

    fn remove(&mut self, song_id: &str) -> bool {
        let Some(slot) = self.index.remove(song_id) else {
            return false;
        };
        self.slots[slot] = None;
        self.free.push(slot);
        true
    }

    fn values(&self) -> impl Iterator<Item = &LyricsPin> {
        self.slots.iter().flatten()
    }

    fn len(&self) -> usize {
        self.index.len()
    }
}

/// The pins, on disk beside the settings. Written whole on every change, through a temporary
/// file, since a pin is a deliberate act that happens a few times a day at most.
pub struct LyricsChoiceStore {
    path: Option<PathBuf>,
    pins: Mutex<Pins>,
}

impl LyricsChoiceStore {
    /// `path` None (or blank) keeps the pins in memory only.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.filter(|path| !is_blank(&path.to_string_lossy()));
        let pins = path.as_deref().map(load).unwrap_or_default();
        Self {
            path,
            pins: Mutex::new(pins),
        }
    }

    pub fn get(&self, song_id: &str) -> Option<LyricsPin> {
        self.pins.lock().get(song_id).cloned()
    }

    /// A pin for the song with this artist and title, whatever its id, and however the
    /// two are written ("Drake feat. Rihanna" or "Too Good (feat. Rihanna)"). Never a pin for
    /// another version of it.
    pub fn find_by_name(&self, artist: &str, title: &str) -> Option<LyricsPin> {
        if SongIdentity::key(artist).is_empty() || SongIdentity::key(title).is_empty() {
            return None;
        }
        let want = SongIdentity::match_key(artist, title);
        let pins = self.pins.lock();
        // OrderByDescending(SetUtc).FirstOrDefault(): the newest, the first of equals.
        let mut best: Option<&LyricsPin> = None;
        for pin in pins.values().filter(|pin| match_key(pin) == want) {
            if best.is_none_or(|best| pin.set_utc > best.set_utc) {
                best = Some(pin);
            }
        }
        best.cloned()
    }

    /// Every pin, newest first.
    pub fn all(&self) -> Vec<LyricsPin> {
        let mut all: Vec<LyricsPin> = self.pins.lock().values().cloned().collect();
        all.sort_by_key(|pin| std::cmp::Reverse(pin.set_utc));
        all
    }

    /// Whether any song has a pin, so a caller can skip looking one up by name.
    pub fn any(&self) -> bool {
        self.pins.lock().len() > 0
    }

    /// Removes every pin for the song with this artist and title, whatever its id.
    pub fn remove_by_name(&self, artist: Option<&str>, title: Option<&str>) -> bool {
        let (artist, title) = (artist.unwrap_or(""), title.unwrap_or(""));
        if SongIdentity::key(artist).is_empty() || SongIdentity::key(title).is_empty() {
            return false;
        }
        let want = SongIdentity::match_key(artist, title);
        let mut pins = self.pins.lock();
        let ids: Vec<String> = pins
            .values()
            .filter(|pin| match_key(pin) == want)
            .map(|pin| pin.song_id.clone())
            .collect();
        if ids.is_empty() {
            return false;
        }
        for id in &ids {
            pins.remove(id);
        }
        self.save(&pins);
        true
    }

    pub fn set(&self, pin: LyricsPin) {
        let mut pins = self.pins.lock();
        pins.set(pin);
        self.save(&pins);
    }

    pub fn remove(&self, song_id: &str) -> bool {
        let mut pins = self.pins.lock();
        if !pins.remove(song_id) {
            return false;
        }
        self.save(&pins);
        true
    }

    fn save(&self, pins: &Pins) {
        let Some(path) = &self.path else {
            return;
        };
        let values: Vec<&LyricsPin> = pins.values().collect();
        let json = octo_core::json::to_string(&values);
        if let Err(error) = state_file::write_atomic(path, json.as_bytes()) {
            warn!("lyrics choices could not be written: {error}");
        }
    }
}

fn match_key(pin: &LyricsPin) -> String {
    SongIdentity::match_key(
        pin.artist.as_deref().unwrap_or(""),
        pin.title.as_deref().unwrap_or(""),
    )
}

/// Duplicate song ids: the last one wins, in the place of the first. Empty ids are dropped.
fn load(path: &Path) -> Pins {
    let mut pins = Pins::default();
    if !path.exists() {
        return pins;
    }
    let read = state_file::read_all_text(path)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            serde_json::from_str::<Option<Vec<LyricsPin>>>(&text).map_err(|error| error.to_string())
        });
    match read {
        Ok(read) => {
            for pin in read
                .unwrap_or_default()
                .into_iter()
                .filter(|pin| !pin.song_id.is_empty())
            {
                pins.set(pin);
            }
        }
        // Unreadable pins are no pins, never a failure to start.
        Err(error) => warn!("lyrics choices could not be read: {error}"),
    }
    pins
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};

    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a date")
            .with_timezone(&Utc)
    }

    fn pin(song_id: &str, choice: &str, artist: &str, title: &str, set_utc: DateTime<Utc>) -> LyricsPin {
        LyricsPin::new(
            song_id,
            choice,
            Some("KuGou".into()),
            Some("[00:01.00]x".into()),
            None,
            Some(artist.into()),
            Some(title.into()),
            Some("alice".into()),
            set_utc,
        )
    }

    /// LyricsChoiceTests.Store_PinsSurviveARestart.
    #[test]
    fn store_pins_survive_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lyrics-choices.json");
        let store = LyricsChoiceStore::new(Some(path.clone()));
        store.set(pin("s1", "kugou:1.a", "A", "T", Utc::now()));
        store.set(LyricsPin::new(
            "s2",
            LyricsPin::HIDDEN,
            None,
            None,
            None,
            Some("B".into()),
            Some("U".into()),
            Some("bob".into()),
            Utc::now(),
        ));

        let again = LyricsChoiceStore::new(Some(path.clone()));

        assert_eq!(again.get("s1").expect("pinned").choice, "kugou:1.a");
        assert!(again.get("s2").expect("hidden").is_hidden());
        assert_eq!(again.find_by_name("a", "t").expect("by name").song_id, "s1");
        assert!(again.remove("s1"));
        assert!(LyricsChoiceStore::new(Some(path)).get("s1").is_none());
    }

    /// The store writes the fixture's pins back byte for byte, in the order it holds them.
    #[test]
    fn the_fixture_reads_and_is_written_back_unchanged() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/lyrics-choices.json"
        );
        let text = std::fs::read_to_string(fixture).expect("the fixture is in the repo");
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lyrics-choices.json");
        std::fs::write(&path, &text).expect("copied");

        let store = LyricsChoiceStore::new(Some(path.clone()));
        let first = store.get("nd-77aa88bb").expect("the first pin");
        // Setting a pin to itself rewrites the file.
        store.set(first);

        assert_eq!(
            std::fs::read_to_string(&path).expect("written"),
            text.trim_end_matches('\n')
        );
        assert!(!state_file::temp_path(&path).exists());
    }

    /// A removed pin's place is taken by the next one added, as .NET's Dictionary reused the
    /// slot, so the file lists the pins in the order the C# store wrote them.
    #[test]
    fn a_removed_pins_place_goes_to_the_next_pin() {
        let store = LyricsChoiceStore::new(None);
        let now = at("2026-10-03T12:00:00Z");
        for id in ["a", "b", "c"] {
            store.set(pin(id, "x:1", id, "T", now));
        }
        assert!(store.remove("b"));
        assert!(store.remove("a"));
        store.set(pin("d", "x:1", "d", "T", now));
        store.set(pin("e", "x:1", "e", "T", now));
        store.set(pin("f", "x:1", "f", "T", now));
        store.set(pin("c", "x:2", "c", "T", now));

        let order: Vec<String> = store
            .pins
            .lock()
            .values()
            .map(|pin| pin.song_id.clone())
            .collect();
        assert_eq!(order, ["d", "e", "c", "f"]);
        assert_eq!(store.get("c").expect("replaced").choice, "x:2");
    }

    #[test]
    fn duplicates_on_disk_keep_the_last_and_empty_ids_are_dropped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lyrics-choices.json");
        std::fs::write(
            &path,
            r#"[{"SongId":"a","Choice":"x:1","SetUtc":"2026-10-03T12:00:00Z"},{"SongId":"","Choice":"x:9","SetUtc":"2026-10-03T12:00:00Z"},{"SongId":null,"Choice":"x:8","SetUtc":"2026-10-03T12:00:00Z"},{"SongId":"b","Choice":"x:2","SetUtc":"2026-10-03T12:00:00Z"},{"SongId":"a","Choice":"x:3","SetUtc":"2026-10-03T12:00:00Z"}]"#,
        )
        .expect("written");

        let store = LyricsChoiceStore::new(Some(path));

        let order: Vec<(String, String)> = store
            .pins
            .lock()
            .values()
            .map(|pin| (pin.song_id.clone(), pin.choice.clone()))
            .collect();
        assert_eq!(
            order,
            [
                ("a".to_string(), "x:3".to_string()),
                ("b".to_string(), "x:2".to_string())
            ]
        );
    }

    #[test]
    fn an_unreadable_file_is_no_pins() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lyrics-choices.json");
        std::fs::write(&path, "{ not pins").expect("written");
        assert!(!LyricsChoiceStore::new(Some(path)).any());
    }

    #[test]
    fn by_name_finds_the_newest_pin_of_the_song_and_never_another_version() {
        let store = LyricsChoiceStore::new(None);
        let now = at("2026-10-03T12:00:00Z");
        store.set(pin("old", "x:1", "Drake feat. Rihanna", "Too Good", now));
        store.set(pin(
            "new",
            "x:2",
            "Drake",
            "Too Good (feat. Rihanna)",
            now + TimeDelta::minutes(1),
        ));
        store.set(pin(
            "live",
            "x:3",
            "Drake",
            "Too Good (Live)",
            now + TimeDelta::minutes(2),
        ));

        assert_eq!(
            store.find_by_name("Drake", "Too Good").expect("found").song_id,
            "new"
        );
        assert!(store.find_by_name("", "Too Good").is_none());
        assert_eq!(
            store
                .all()
                .iter()
                .map(|pin| pin.song_id.as_str())
                .collect::<Vec<_>>(),
            ["live", "new", "old"]
        );

        assert!(store.remove_by_name(Some("drake"), Some("too good")));
        assert!(store.get("old").is_none() && store.get("new").is_none());
        assert!(store.get("live").is_some());
        assert!(!store.remove_by_name(None, Some("Too Good")));
    }
}
