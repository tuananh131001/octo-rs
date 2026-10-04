//! Port of `LyricsChoiceStore` and `LyricsChoiceService` from `Services/Lyrics/LyricsChoices.cs`:
//! the pins on disk (`lyrics-choices.json`), and choosing lyrics by hand. The pin and the entry
//! offered are `octo_core::lyrics::{LyricsPin, LyricsChoiceCandidate}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{is_blank, is_null_or_white_space};
use octo_core::lyrics::{
    ILyricsSource, LyricsCandidate, LyricsChoiceCandidate, LyricsPin, LyricsQuery, LyricsResult,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::lyrics_service::LyricsService;
use super::memory_cache::MemoryCache;
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

/// Choosing lyrics by hand, for the Octo app's "Choose other lyrics" (getLyricsCandidates and
/// setLyricsChoice) and the dashboard's picker, which are two doors to the same pins.
pub struct LyricsChoiceService {
    lyrics: Arc<LyricsService>,
    store: Arc<LyricsChoiceStore>,
    fetched: MemoryCache<LyricsResult>,
}

impl LyricsChoiceService {
    /// How many entries per source are fetched for a preview. Each is a request, and KuGou and
    /// NetEase need one per entry.
    const PER_SOURCE: usize = 4;

    const REMEMBERED: Duration = Duration::from_secs(30 * 60);

    pub fn new(lyrics: Arc<LyricsService>, store: Arc<LyricsChoiceStore>) -> Self {
        Self {
            lyrics,
            store,
            fetched: MemoryCache::new(1024),
        }
    }

    /// `PinFor(songId)`.
    pub fn pin_for(&self, song_id: &str) -> Option<LyricsPin> {
        self.store.get(song_id)
    }

    /// `PinFor(artist, title)`.
    pub fn pin_for_name(&self, artist: &str, title: &str) -> Option<LyricsPin> {
        self.store.find_by_name(artist, title)
    }

    /// `PinFor(songId, artist, title)`: the song's pin, by its id, or else by its artist and
    /// title. Navidrome gives a song a new id when its file is replaced (a better copy, a
    /// move), and the pin follows the song.
    pub fn pin_for_song(
        &self,
        song_id: &str,
        artist: Option<&str>,
        title: Option<&str>,
    ) -> Option<LyricsPin> {
        self.store.get(song_id).or_else(|| match (artist, title) {
            (Some(artist), Some(title))
                if !is_null_or_white_space(Some(artist)) && !is_null_or_white_space(Some(title)) =>
            {
                self.store.find_by_name(artist, title)
            }
            _ => None,
        })
    }

    /// Whether any song has a pin.
    pub fn any_pins(&self) -> bool {
        self.store.any()
    }

    /// `ChoiceFor(songId)`: "auto" when nothing is chosen, "none" when hidden, else the
    /// candidate id.
    pub fn choice_for(&self, song_id: &str) -> String {
        self.store
            .get(song_id)
            .map_or_else(|| LyricsPin::AUTO.to_string(), |pin| pin.choice)
    }

    /// `ChoiceFor(songId, artist, title)`: the song's choice, found by its id or else by its
    /// artist and title.
    pub fn choice_for_song(&self, song_id: &str, artist: Option<&str>, title: Option<&str>) -> String {
        self.pin_for_song(song_id, artist, title)
            .map_or_else(|| LyricsPin::AUTO.to_string(), |pin| pin.choice)
    }

    pub fn all(&self) -> Vec<LyricsPin> {
        self.store.all()
    }

    /// Every entry the sources that are on hold for the song, the same song first, each with
    /// its lyrics fetched for a preview. Sources are asked side by side; entries not fetched
    /// when `ct` runs out are left out, since without their lyrics there is nothing to choose by.
    pub async fn candidates(
        &self,
        query: &LyricsQuery,
        ct: &CancellationToken,
    ) -> Vec<LyricsChoiceCandidate> {
        let sources = self.lyrics.enabled();
        let per_source = futures::future::join_all(
            sources
                .iter()
                .map(|source| self.candidates_from(source.as_ref(), query, ct)),
        )
        .await;
        per_source.into_iter().flatten().collect()
    }

    async fn candidates_from(
        &self,
        source: &dyn ILyricsSource,
        query: &LyricsQuery,
        ct: &CancellationToken,
    ) -> Vec<LyricsChoiceCandidate> {
        let mut offered = Vec::new();
        // A source never throws here: a failed or cancelled search is simply no entries, as the
        // C# catch made of it.
        let search = source.search(query, ct).await;
        let mut ranked: Vec<(&LyricsCandidate, bool, i32)> = search
            .candidates
            .iter()
            .map(|candidate| {
                let distance = match (
                    query.duration_seconds.filter(|d| *d > 0),
                    candidate.duration_seconds.filter(|d| *d > 0),
                ) {
                    (Some(want), Some(got)) => (got - want).abs(),
                    _ => 0,
                };
                (
                    candidate,
                    LyricsChoiceCandidate::is_this_song(candidate, query),
                    distance,
                )
            })
            .collect();
        // OrderByDescending(same).ThenBy(distance): stable.
        ranked.sort_by_key(|(_, same, distance)| (std::cmp::Reverse(*same), *distance));

        for (candidate, same, _) in ranked.into_iter().take(Self::PER_SOURCE) {
            if ct.is_cancelled() {
                break;
            }
            let lyrics = match &candidate.lyrics {
                Some(lyrics) => Some(lyrics.clone()),
                None => source.fetch(&candidate.id, ct).await.result,
            };
            let Some(lyrics) = lyrics else {
                continue;
            };
            let candidate_id = candidate.candidate_id();
            self.fetched.set(
                &candidate_id,
                lyrics.clone().with_candidate_id(candidate_id.clone()),
                Self::REMEMBERED,
            );
            offered.push(LyricsChoiceCandidate::offered(
                candidate,
                source.key(),
                &lyrics,
                same,
            ));
        }
        offered
    }

    /// `KindOf`: "word", "line", "plain" or "instrumental".
    pub fn kind_of(lyrics: &LyricsResult) -> &'static str {
        LyricsChoiceCandidate::kind_of(lyrics)
    }

    /// The lyrics of one candidate: from the list just shown when it is still remembered,
    /// otherwise asked of its source again. (When `ct` runs out the C# threw; this is None.)
    pub async fn lyrics_of(&self, candidate_id: &str, ct: &CancellationToken) -> Option<LyricsResult> {
        if let Some(known) = self.fetched.get(candidate_id) {
            return Some(known);
        }
        let colon = candidate_id.find(':').filter(|colon| *colon > 0)?;
        let source = self.lyrics.source(&candidate_id[..colon])?;
        let lookup = source.fetch(&candidate_id[colon + 1..], ct).await;
        lookup.result.map(|result| result.with_candidate_id(candidate_id))
    }

    /// Pin a song to a candidate. False when the candidate's lyrics cannot be had.
    pub async fn pin(
        &self,
        song_id: &str,
        candidate_id: &str,
        artist: Option<&str>,
        title: Option<&str>,
        who: Option<&str>,
        ct: &CancellationToken,
    ) -> bool {
        let Some(lyrics) = self.lyrics_of(candidate_id, ct).await else {
            return false;
        };
        if !lyrics.has_synced() && !lyrics.has_plain() {
            return false;
        }
        self.store.set(LyricsPin::new(
            song_id,
            candidate_id,
            Some(lyrics.source.clone()),
            lyrics.synced.clone(),
            lyrics.plain.clone(),
            artist.map(str::to_string),
            title.map(str::to_string),
            who.map(str::to_string),
            Utc::now(),
        ));
        info!(
            "Lyrics for '{} - {}' pinned to {candidate_id} by {}",
            artist.unwrap_or(""),
            title.unwrap_or(""),
            who.unwrap_or("the dashboard")
        );
        true
    }

    pub fn hide(&self, song_id: &str, artist: Option<&str>, title: Option<&str>, who: Option<&str>) {
        self.store.set(LyricsPin::new(
            song_id,
            LyricsPin::HIDDEN,
            None,
            None,
            None,
            artist.map(str::to_string),
            title.map(str::to_string),
            who.map(str::to_string),
            Utc::now(),
        ));
        info!(
            "Lyrics for '{} - {}' hidden by {}",
            artist.unwrap_or(""),
            title.unwrap_or(""),
            who.unwrap_or("the dashboard")
        );
    }

    /// `Clear(songId)`.
    pub fn clear(&self, song_id: &str) -> bool {
        self.store.remove(song_id)
    }

    /// `Clear(songId, artist, title)`: back to automatic, the song's pin by id, and any it has
    /// by its artist and title, so a pin made under an older id does not come back.
    pub fn clear_song(&self, song_id: &str, artist: Option<&str>, title: Option<&str>) -> bool {
        // Both run, as C#'s non-short-circuiting `|` did.
        let by_id = self.store.remove(song_id);
        let by_name = self.store.remove_by_name(artist, title);
        by_id | by_name
    }
}

#[cfg(test)]
#[path = "lyrics_choice_service_tests.rs"]
mod service_tests;

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
