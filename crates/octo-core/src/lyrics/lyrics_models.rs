//! Port of `Services/Lyrics/LyricsModels.cs`: the query, the answer, and the source interface.

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};
use tokio_util::sync::CancellationToken;

use super::lyrics_text::LyricsText;
use crate::common::dotnet::is_null_or_white_space;
use crate::settings::MetadataSettings;

/// What a lyrics lookup asks for. Note the C# parameter order: artist first.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LyricsQuery {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
}

impl LyricsQuery {
    pub fn new(
        artist: impl Into<String>,
        title: impl Into<String>,
        album: Option<String>,
        duration_seconds: Option<i32>,
    ) -> Self {
        Self {
            artist: artist.into(),
            title: title.into(),
            album,
            duration_seconds,
        }
    }
}

/// How much timing a lyric carries, best last, so a comparison ranks them.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize_repr, Deserialize_repr,
)]
#[repr(i32)]
pub enum LyricsTiming {
    #[default]
    None = 0,
    Plain = 1,
    Line = 2,
    Word = 3,
}

/// Synced is LRC text with timestamps, and when a source has word timing it is enhanced LRC:
/// the standard line tags plus a `<mm:ss.xx>` tag before each word, which any player that
/// reads .lrc still shows line by line. Plain is untimed text. Instrumental means the source
/// knows the track has no words, which is an answer, not a miss.
///
/// Written to `lyrics-choices.json` and `lyrics-library.json` with its computed properties
/// (`HasSynced`, `HasPlain`, `HasWordTiming`, `Timing`, `IsSongsOwn`), as System.Text.Json
/// wrote every public getter; those are ignored on read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricsResult {
    pub source: String,
    pub synced: Option<String>,
    pub plain: Option<String>,
    pub instrumental: bool,

    /// Set on a stand-in for the lyrics the song already has (in its tags or a file beside it),
    /// which Navidrome serves and Octo only ranks: how they are timed. The text stays Navidrome's.
    pub song_timing: Option<LyricsTiming>,

    /// The candidate these lyrics came from ("kugou:..."), so a pin can name it.
    pub candidate_id: Option<String>,

    /// Why the match is not certain (the length could not be checked, or is close to
    /// the edge), for the library job's review list. None when nothing is in doubt.
    pub doubt: Option<String>,
}

impl LyricsResult {
    pub fn new(
        source: impl Into<String>,
        synced: Option<String>,
        plain: Option<String>,
        instrumental: bool,
    ) -> Self {
        Self {
            source: source.into(),
            synced,
            plain,
            instrumental,
            song_timing: None,
            candidate_id: None,
            doubt: None,
        }
    }

    pub fn has_synced(&self) -> bool {
        !is_null_or_white_space(self.synced.as_deref())
    }

    pub fn has_plain(&self) -> bool {
        !is_null_or_white_space(self.plain.as_deref())
    }

    pub fn has_word_timing(&self) -> bool {
        self.has_synced() && LyricsText::has_word_tags(self.synced.as_deref())
    }

    pub fn timing(&self) -> LyricsTiming {
        if let Some(song) = self.song_timing {
            song
        } else if self.instrumental {
            LyricsTiming::None
        } else if self.has_word_timing() {
            LyricsTiming::Word
        } else if self.has_synced() {
            LyricsTiming::Line
        } else if self.has_plain() {
            LyricsTiming::Plain
        } else {
            LyricsTiming::None
        }
    }

    /// Whether this stands in for the song's own lyrics: serve Navidrome's.
    pub fn is_songs_own(&self) -> bool {
        self.song_timing.is_some()
    }

    /// A stand-in for the song's own lyrics, timed as given.
    pub fn songs_own(timing: LyricsTiming) -> Self {
        Self {
            song_timing: Some(timing),
            ..Self::new(MetadataSettings::SONG_LYRICS_SOURCE, None, None, false)
        }
    }

    /// `result with { CandidateId = id }`.
    pub fn with_candidate_id(mut self, candidate_id: impl Into<String>) -> Self {
        self.candidate_id = Some(candidate_id.into());
        self
    }
}

/// The shape System.Text.Json wrote: the record's parameters, its computed getters and its
/// init properties, in declaration order.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LyricsResultOut<'a> {
    source: &'a str,
    synced: &'a Option<String>,
    plain: &'a Option<String>,
    instrumental: bool,
    has_synced: bool,
    has_plain: bool,
    has_word_timing: bool,
    timing: LyricsTiming,
    song_timing: Option<LyricsTiming>,
    is_songs_own: bool,
    candidate_id: &'a Option<String>,
    doubt: &'a Option<String>,
}

impl Serialize for LyricsResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        LyricsResultOut {
            source: &self.source,
            synced: &self.synced,
            plain: &self.plain,
            instrumental: self.instrumental,
            has_synced: self.has_synced(),
            has_plain: self.has_plain(),
            has_word_timing: self.has_word_timing(),
            timing: self.timing(),
            song_timing: self.song_timing,
            is_songs_own: self.is_songs_own(),
            candidate_id: &self.candidate_id,
            doubt: &self.doubt,
        }
        .serialize(serializer)
    }
}

/// What a read takes: the settable members only. The computed ones are ignored, as STJ did.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct LyricsResultIn {
    // A null where C# declared a non-nullable string reads as empty.
    #[serde(deserialize_with = "crate::models::null_as_default")]
    source: String,
    synced: Option<String>,
    plain: Option<String>,
    instrumental: bool,
    song_timing: Option<LyricsTiming>,
    candidate_id: Option<String>,
    doubt: Option<String>,
}

impl<'de> Deserialize<'de> for LyricsResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let read = LyricsResultIn::deserialize(deserializer)?;
        Ok(Self {
            source: read.source,
            synced: read.synced,
            plain: read.plain,
            instrumental: read.instrumental,
            song_timing: read.song_timing,
            candidate_id: read.candidate_id,
            doubt: read.doubt,
        })
    }
}

/// A source's answer. Transient means it could not answer right now (rate limited, overloaded,
/// timed out), which is never remembered as "this song has no lyrics": LRCLIB sheds load with
/// 503s often enough that caching those as misses would blank songs for no reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsLookup {
    pub result: Option<LyricsResult>,
    pub transient: bool,
}

impl LyricsLookup {
    pub fn new(result: Option<LyricsResult>, transient: bool) -> Self {
        Self { result, transient }
    }

    /// `LyricsLookup.Miss`.
    pub fn miss() -> Self {
        Self::new(None, false)
    }

    /// `LyricsLookup.Failed`.
    pub fn failed() -> Self {
        Self::new(None, true)
    }
}

/// One entry a source's search returned: which song it says it is, before its lyrics are
/// fetched. Id is the source's own and opaque; CandidateId adds the source, so it can be
/// handed to a client and come back in setLyricsChoice. Lyrics is filled when the search
/// answer already carried them (LRCLIB does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsCandidate {
    pub source: String,
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    pub lyrics: Option<LyricsResult>,
}

impl LyricsCandidate {
    pub fn new(
        source: impl Into<String>,
        id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        album: Option<String>,
        duration_seconds: Option<i32>,
    ) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            album,
            duration_seconds,
            lyrics: None,
        }
    }

    pub fn candidate_id(&self) -> String {
        format!("{}:{}", self.source, self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricsSearch {
    pub candidates: Vec<LyricsCandidate>,
    pub transient: bool,
}

impl LyricsSearch {
    pub fn new(candidates: Vec<LyricsCandidate>, transient: bool) -> Self {
        Self {
            candidates,
            transient,
        }
    }

    /// `LyricsSearch.Empty`.
    pub fn empty() -> Self {
        Self::new(Vec::new(), false)
    }

    /// `LyricsSearch.Failed`.
    pub fn failed() -> Self {
        Self::new(Vec::new(), true)
    }
}

#[async_trait]
pub trait ILyricsSource: Send + Sync {
    /// The name in LYRICS_SOURCES: kugou, lrclib, netease or lyricsovh.
    fn key(&self) -> &str;

    /// The lyrics of the one entry that is this song, or a miss.
    async fn find(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsLookup;

    /// Every entry the source's search returns for the song, same song or not, for a
    /// person choosing between them. Nothing is filtered here; identity is the caller's call.
    async fn search(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsSearch {
        LyricsSearch::empty()
    }

    /// The lyrics of one entry by the id its search gave.
    async fn fetch(&self, _id: &str, _ct: &CancellationToken) -> LyricsLookup {
        LyricsLookup::miss()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_ranks_word_above_line_above_plain() {
        assert!(LyricsTiming::Word > LyricsTiming::Line);
        assert!(LyricsTiming::Line > LyricsTiming::Plain);
        assert!(LyricsTiming::Plain > LyricsTiming::None);
    }

    #[test]
    fn the_songs_own_stand_in_reports_its_timing_and_names_the_song_source() {
        let own = LyricsResult::songs_own(LyricsTiming::Word);
        assert_eq!(own.source, "song");
        assert!(own.is_songs_own());
        assert_eq!(own.timing(), LyricsTiming::Word);
        assert!(!own.has_synced());

        let instrumental = LyricsResult::new("x", Some("[00:01.00]a".into()), None, true);
        assert_eq!(instrumental.timing(), LyricsTiming::None);
        assert_eq!(
            LyricsResult::new("x", None, Some(" ".into()), false).timing(),
            LyricsTiming::None
        );
    }

    #[test]
    fn a_result_writes_its_computed_members_and_reads_without_them() {
        let result = LyricsResult::new("KuGou", Some("[00:01.00]<00:01.00>a".into()), None, false)
            .with_candidate_id("kugou:1");
        let json = crate::json::to_string(&result);
        assert_eq!(
            json,
            r#"{"Source":"KuGou","Synced":"[00:01.00]\u003C00:01.00\u003Ea","Plain":null,"Instrumental":false,"HasSynced":true,"HasPlain":false,"HasWordTiming":true,"Timing":3,"SongTiming":null,"IsSongsOwn":false,"CandidateId":"kugou:1","Doubt":null}"#
        );
        let back: LyricsResult = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, result);

        let own: LyricsResult =
            serde_json::from_str(r#"{"Source":null,"SongTiming":2,"Timing":0}"#).expect("reads");
        assert_eq!(own.source, "");
        assert_eq!(own.timing(), LyricsTiming::Line);
    }

    #[test]
    fn a_candidate_id_names_its_source() {
        let candidate = LyricsCandidate::new("kugou", "9.KEY9", "Stronger", "Kanye West", None, Some(312));
        assert_eq!(candidate.candidate_id(), "kugou:9.KEY9");
    }
}
