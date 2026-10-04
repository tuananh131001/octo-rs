//! Lyrics with I/O (`Services/Lyrics`): the pins on disk and what a song file has now. The
//! models, LRC handling and matching are `octo_core::lyrics`.

pub mod lyrics_choices;
pub mod song_lyrics;

pub use lyrics_choices::LyricsChoiceStore;
