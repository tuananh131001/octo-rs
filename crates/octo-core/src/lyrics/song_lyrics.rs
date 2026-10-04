//! Port of `Services/Lyrics/SongLyrics.cs`, its pure half: the answer and how text is timed.
//! The half that looks at files (`SongLyrics.Of`, `MayWriteInside`, `MayWriteBeside`) is
//! `octo::services::lyrics::song_lyrics`.

use super::lyrics_models::LyricsTiming;
use super::lyrics_text::LyricsText;
use crate::common::dotnet::is_null_or_white_space;

/// `LyricsSidecarWriter.OctoMark`: the first line of every lyrics file Octo writes, and of the
/// lyrics it writes inside a song's tags. Octo replaces only lyrics that start with it.
pub const OCTO_MARK: &str = "[re:Octo]";

/// Where a song's lyrics are: nowhere, in a file beside it, or in its own tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SongLyricsPlace {
    #[default]
    None,
    Beside,
    Inside,
}

/// The lyrics a song file has now, as Navidrome serves them: a .lrc beside it first, then a .txt,
/// then its tags (Navidrome's default LyricsPriority). Octos is whether Octo wrote them, so may
/// replace them. Unknown is a lyrics file in a format Octo does not read, which it leaves alone.
///
/// (C#'s `Where` is `place` here, `where` being a Rust keyword.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SongLyrics {
    pub place: SongLyricsPlace,
    pub timing: LyricsTiming,
    pub octos: bool,
    pub unknown: bool,
}

impl SongLyrics {
    /// `SongLyrics.Nothing`.
    pub const NOTHING: SongLyrics = SongLyrics {
        place: SongLyricsPlace::None,
        timing: LyricsTiming::None,
        octos: false,
        unknown: false,
    };

    /// Lyrics files Navidrome or another player may read beside a song, besides .lrc and .txt.
    pub const OTHER_EXTENSIONS: [&'static str; 5] = [".ttml", ".elrc", ".srt", ".yaml", ".yml"];

    pub fn new(place: SongLyricsPlace, timing: LyricsTiming, octos: bool, unknown: bool) -> Self {
        Self {
            place,
            timing,
            octos,
            unknown,
        }
    }

    /// How lyrics text is timed: word tags, line tags, or neither.
    pub fn timing_of(text: Option<&str>) -> LyricsTiming {
        if is_null_or_white_space(text) {
            LyricsTiming::None
        } else if LyricsText::has_word_tags(text) {
            LyricsTiming::Word
        } else if LyricsText::has_timestamps(text) {
            LyricsTiming::Line
        } else {
            LyricsTiming::Plain
        }
    }

    /// Whether lyrics text (from a song's tags) is Octo's: its first line, byte order mark and
    /// spaces aside, is [`OCTO_MARK`].
    pub fn is_octos_text(text: &str) -> bool {
        let text = text.trim_start_matches('\u{FEFF}');
        text.split('\n').next().unwrap_or("").trim() == OCTO_MARK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_of_reads_word_tags_then_line_tags_then_text() {
        assert_eq!(SongLyrics::timing_of(None), LyricsTiming::None);
        assert_eq!(SongLyrics::timing_of(Some(" \n ")), LyricsTiming::None);
        assert_eq!(
            SongLyrics::timing_of(Some("[00:01.00]<00:01.00>a")),
            LyricsTiming::Word
        );
        assert_eq!(SongLyrics::timing_of(Some("[00:01.00]a")), LyricsTiming::Line);
        assert_eq!(SongLyrics::timing_of(Some("a")), LyricsTiming::Plain);
    }

    #[test]
    fn octos_text_starts_with_the_mark() {
        assert!(SongLyrics::is_octos_text("[re:Octo]\n[00:01.00]x"));
        assert!(SongLyrics::is_octos_text("\u{FEFF} [re:Octo] \r\nx"));
        assert!(!SongLyrics::is_octos_text("x\n[re:Octo]"));
        assert!(!SongLyrics::is_octos_text(""));
        assert_eq!(SongLyrics::default(), SongLyrics::NOTHING);
    }
}
