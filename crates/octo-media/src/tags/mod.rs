//! Audio tags: what TagLibSharp 2.3.0 wrote and read for the C# build, written and read with
//! lofty.
//!
//! C# sources: `Services/Common/TagWriterExtras.cs`, the tag-writing parts of
//! `Services/Common/BaseDownloadService.cs`, and `Services/Library/KeptIdentity.cs`.
//! [`TagFile`] stands for `TagLib.File` and its generic `Tag`: each container's own tag is
//! kept in TagLib#'s model (ID3v2 frames, a Vorbis comment's fields, an iTunes item list's
//! boxes) and rendered the way TagLib# renders it, so a file Rust tags reads back as the one
//! C# tagged. The differences lofty forces are in `docs/rust-migration/known-diffs.md`, and
//! `fixture_tests` compares both builds' files frame by frame.

mod apple;
mod genres;
mod id3;
mod net;
mod net_dictionary;
mod xiph;

pub mod kept_identity;
pub mod song_tags;
pub mod tag_file;
pub mod tag_writer_extras;

#[cfg(test)]
mod dump;
#[cfg(test)]
mod fixture_tests;
#[cfg(test)]
mod kept_identity_tests;
#[cfg(test)]
mod tag_writer_extras_tests;
#[cfg(test)]
pub(crate) mod test_support;

pub use kept_identity::{KeptIdentity, KeptIdentityTags};
pub use song_tags::{
    GenreWrite, embed_cover, front_cover, read_lyrics, write_album_gain, write_lyrics, write_song,
};
pub use tag_file::{FRONT_COVER, Format, TagError, TagFile, TagPicture};
pub use tag_writer_extras::{TagField, TagFields, TagWriterExtras};
