//! Lyrics, the pure parts (`Services/Lyrics`): the models, LRC reading and writing, the rule for
//! which entry is the song, the Lyricsfile reader, what a song file has now, and the pin model.
//! The sources, the service, the sidecar writer and the pin store live in `octo` under
//! `octo::services::lyrics`.
//!
//! **Text positions.** C# placed a word in its line with UTF-16 indices (`LyricWord.From`/`To`).
//! The port uses UTF-8 byte offsets into `LyricLine::text`, so a word is `&text[from..to]`
//! and the OpenSubsonic cues (UTF-8 byte offsets) need no conversion. The positions are never
//! written anywhere, so nothing outside can tell.

pub mod lyrics_choices;
pub mod lyrics_identity;
pub mod lyrics_models;
pub mod lyrics_text;
pub mod lyricsfile_reader;
pub mod song_lyrics;

pub use lyrics_choices::{LyricsChoiceCandidate, LyricsPin};
pub use lyrics_identity::LyricsIdentity;
pub use lyrics_models::{
    ILyricsSource, LyricsCandidate, LyricsLookup, LyricsQuery, LyricsResult, LyricsSearch, LyricsTiming,
};
pub use lyrics_text::{LyricLine, LyricWord, LyricsText};
pub use lyricsfile_reader::LyricsfileReader;
pub use song_lyrics::{SongLyrics, SongLyricsPlace};
