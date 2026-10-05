//! Port of `Services/Lyrics/LyricsChoices.cs`, its pure half: the pin, the entry offered to a
//! person choosing, and how an entry is judged and described. The pins on disk
//! (`LyricsChoiceStore`) are `octo::services::lyrics::lyrics_choices`; `LyricsChoiceService`,
//! which asks the sources, comes with them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};

use super::lyrics_identity::LyricsIdentity;
use super::lyrics_models::{LyricsCandidate, LyricsQuery, LyricsResult, LyricsTiming};
use super::lyrics_text::LyricsText;

/// What someone chose for one song's lyrics: a candidate, kept with its lyrics so the pin still
/// answers when the source is down or has changed, or none, which hides the song's lyrics. Held
/// for the whole server, so every client sees the same choice, and named by artist and title
/// too, so the legacy getLyrics call (which knows nothing else) honours it.
///
/// One entry of `lyrics-choices.json`, written with its computed `IsHidden` and `Lyrics`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LyricsPin {
    // The non-nullable C# strings read a JSON null as empty (STJ kept the null).
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub song_id: String,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub choice: String,
    pub source: Option<String>,
    pub synced: Option<String>,
    pub plain: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub set_by: Option<String>,
    #[serde(with = "crate::json::datetime::utc")]
    pub set_utc: DateTime<Utc>,
}

impl Default for LyricsPin {
    /// What a missing member reads as: `default(T)` for the record's parameters.
    fn default() -> Self {
        Self {
            song_id: String::new(),
            choice: String::new(),
            source: None,
            synced: None,
            plain: None,
            artist: None,
            title: None,
            set_by: None,
            set_utc: crate::json::datetime::min_value(),
        }
    }
}

impl LyricsPin {
    /// The choice that hides a song's lyrics.
    pub const HIDDEN: &'static str = "none";
    /// What a song with no pin answers.
    pub const AUTO: &'static str = "auto";

    pub fn new(
        song_id: impl Into<String>,
        choice: impl Into<String>,
        source: Option<String>,
        synced: Option<String>,
        plain: Option<String>,
        artist: Option<String>,
        title: Option<String>,
        set_by: Option<String>,
        set_utc: DateTime<Utc>,
    ) -> Self {
        Self {
            song_id: song_id.into(),
            choice: choice.into(),
            source,
            synced,
            plain,
            artist,
            title,
            set_by,
            set_utc,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.choice == Self::HIDDEN
    }

    pub fn lyrics(&self) -> Option<LyricsResult> {
        if self.is_hidden() {
            return None;
        }
        Some(
            LyricsResult::new(
                self.source.clone().unwrap_or_else(|| "pinned".to_string()),
                self.synced.clone(),
                self.plain.clone(),
                false,
            )
            .with_candidate_id(self.choice.clone()),
        )
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LyricsPinOut<'a> {
    song_id: &'a str,
    choice: &'a str,
    source: &'a Option<String>,
    synced: &'a Option<String>,
    plain: &'a Option<String>,
    artist: &'a Option<String>,
    title: &'a Option<String>,
    set_by: &'a Option<String>,
    set_utc: String,
    is_hidden: bool,
    lyrics: Option<LyricsResult>,
}

impl Serialize for LyricsPin {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        LyricsPinOut {
            song_id: &self.song_id,
            choice: &self.choice,
            source: &self.source,
            synced: &self.synced,
            plain: &self.plain,
            artist: &self.artist,
            title: &self.title,
            set_by: &self.set_by,
            set_utc: crate::json::datetime::format_utc(&self.set_utc),
            is_hidden: self.is_hidden(),
            lyrics: self.lyrics(),
        }
        .serialize(serializer)
    }
}

/// One entry offered to a person choosing lyrics, with enough to choose by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsChoiceCandidate {
    pub id: String,
    pub source: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    pub kind: String,
    pub same_song: bool,
    pub preview: Vec<String>,
}

impl LyricsChoiceCandidate {
    /// `LyricsChoiceService.KindOf`: how an entry's lyrics are timed, as the choice list names it.
    pub fn kind_of(lyrics: &LyricsResult) -> &'static str {
        match lyrics.timing() {
            LyricsTiming::Word => "word",
            LyricsTiming::Line => "line",
            LyricsTiming::Plain => "plain",
            LyricsTiming::None => "instrumental",
        }
    }

    /// `LyricsChoiceService.IsThisSong`: whether a search entry is the song asked for, its
    /// artist read one by one where it lists several.
    pub fn is_this_song(candidate: &LyricsCandidate, query: &LyricsQuery) -> bool {
        let credits: Vec<&str> = candidate
            .artist
            .split(['、', ',', '&'])
            .map(str::trim)
            .filter(|credit| !credit.is_empty())
            .collect();
        LyricsIdentity::same_song(
            &query.title,
            &query.artist,
            Some(&candidate.title),
            Some(&candidate.artist),
            Some(&credits),
        ) && LyricsIdentity::length_fits(query.duration_seconds, candidate.duration_seconds.map(f64::from))
    }

    /// The entry as offered: its lyrics (fetched or carried) give its kind and preview.
    pub fn offered(
        candidate: &LyricsCandidate,
        source_key: &str,
        lyrics: &LyricsResult,
        same_song: bool,
    ) -> Self {
        Self {
            id: candidate.candidate_id(),
            source: source_key.to_string(),
            title: candidate.title.clone(),
            artist: candidate.artist.clone(),
            album: candidate.album.clone(),
            duration_seconds: candidate.duration_seconds,
            kind: Self::kind_of(lyrics).to_string(),
            same_song,
            preview: LyricsText::preview(lyrics, 2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/lyrics-choices.json"
        );
        std::fs::read_to_string(path).expect("the fixture is in the repo")
    }

    #[test]
    fn lyrics_choices_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        let pins: Vec<LyricsPin> = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(pins.len(), 2);
        assert!(!pins[0].is_hidden());
        assert_eq!(pins[0].artist.as_deref(), Some("Björk"));
        assert!(pins[1].is_hidden());
        assert!(pins[1].lyrics().is_none());

        assert_eq!(crate::json::to_string(&pins), text.trim_end_matches('\n'));
    }

    #[test]
    fn a_pin_without_a_source_is_pinned_lyrics_named_by_its_choice() {
        let pin = LyricsPin::new(
            "s1",
            "kugou:1.a",
            None,
            Some("[00:01.00]x".into()),
            None,
            None,
            None,
            None,
            Utc::now(),
        );
        let lyrics = pin.lyrics().expect("not hidden");
        assert_eq!(lyrics.source, "pinned");
        assert_eq!(lyrics.candidate_id.as_deref(), Some("kugou:1.a"));
        assert_eq!(LyricsChoiceCandidate::kind_of(&lyrics), "line");
    }

    #[test]
    fn kind_of_names_every_timing() {
        let kind = |synced: Option<&str>, plain: Option<&str>, instrumental: bool| {
            LyricsChoiceCandidate::kind_of(&LyricsResult::new(
                "x",
                synced.map(String::from),
                plain.map(String::from),
                instrumental,
            ))
        };
        assert_eq!(kind(Some("[00:01.00]<00:01.00>a"), None, false), "word");
        assert_eq!(kind(Some("[00:01.00]a"), None, false), "line");
        assert_eq!(kind(None, Some("a"), false), "plain");
        assert_eq!(kind(Some("[00:01.00]a"), None, true), "instrumental");
    }

    #[test]
    fn is_this_song_splits_the_artists_and_checks_the_length() {
        let query = LyricsQuery::new("Lil Yachty", "Peek A Boo", None, Some(200));
        let entry = |artist: &str, seconds: i32| {
            LyricsCandidate::new("kugou", "1", "Peek a Boo", artist, None, Some(seconds))
        };
        assert!(LyricsChoiceCandidate::is_this_song(
            &entry("Migos & Lil Yachty", 201),
            &query
        ));
        assert!(!LyricsChoiceCandidate::is_this_song(
            &entry("Lil Yachty", 230),
            &query
        ));
        assert!(!LyricsChoiceCandidate::is_this_song(
            &entry("Someone Else", 200),
            &query
        ));
    }
}
