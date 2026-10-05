//! `TagLib.File`: an audio file's tags in memory, read and changed through TagLib#'s generic
//! `Tag` properties and saved back with lofty.
//!
//! The format comes from the extension, as TagLib# chose it. Each format carries the tags
//! TagLib# gives it: an MP3 an ID3v2 tag (created at its default version, 3) and an ID3v1
//! tag, a FLAC and an Ogg file a Vorbis comment, an MP4 an iTunes item list, an AIFF or a WAV
//! an ID3v2 tag. The generic properties read the first tag that has a value and write every
//! tag, as TagLib#'s combined tag does.

use std::fs::OpenOptions;
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::file::FileType;
use lofty::flac::FlacFile;
use lofty::id3::v1::Id3v1Tag;
use lofty::id3::v2::Id3v2Tag;
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::Mp4File;
use lofty::mpeg::MpegFile;
use lofty::ogg::{OggPictureStorage, OpusFile, SpeexFile, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use lofty::prelude::*;
use lofty::tag::TagType;

use super::apple::{AppleTag, names};
use super::genres;
use super::id3::Id3Tag;
use super::net::{parse_double, parse_uint};
use super::xiph::XiphComment;

/// Why a file could not be opened or saved (TagLib#'s `CorruptFileException`,
/// `UnsupportedFormatException`, and I/O errors).
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Read(#[from] lofty::error::FileParseError),
    #[error("{0}")]
    Write(#[from] lofty::error::FileEncodingError),
    #[error("{0}")]
    Picture(#[from] lofty::picture::error::PictureParseError),
    #[error("unsupported format: {0}")]
    Unsupported(String),
}

/// The container a file was opened as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Mpeg,
    Flac,
    Ogg,
    Mp4,
    Aiff,
    Wav,
    /// A format whose length lofty reads but whose tags are not handled here (APE, WavPack,
    /// Musepack, ADTS AAC): it reads as untagged and cannot be saved.
    Other,
}

/// TagLib#'s `PictureType.FrontCover`.
pub const FRONT_COVER: u8 = 3;

/// TagLib#'s `IPicture`: what an embedded picture is, its MIME type, description and bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagPicture {
    pub picture_type: u8,
    pub mime_type: String,
    pub description: String,
    pub data: Vec<u8>,
}

impl TagPicture {
    /// The picture the download pipeline embeds: a front cover described as "Cover".
    pub fn front_cover(data: Vec<u8>, mime_type: &str) -> Self {
        Self {
            picture_type: FRONT_COVER,
            mime_type: mime_type.to_string(),
            description: "Cover".into(),
            data,
        }
    }

    /// `new TagLib.Picture(data)`: the type and MIME type from the bytes' signature, described
    /// as "cover.png" and the like.
    fn from_data(data: Vec<u8>) -> Self {
        let extension = if data.len() >= 4 {
            if &data[1..4] == b"PNG" {
                Some("png")
            } else if data.starts_with(b"GIF") {
                Some("gif")
            } else if data.starts_with(b"BM") {
                Some("bmp")
            } else if data[0] == 0xFF
                && data[1] == 0xD8
                && data[data.len() - 2] == 0xFF
                && data[data.len() - 1] == 0xD9
            {
                Some("jpg")
            } else {
                None
            }
        } else {
            None
        };
        match extension {
            Some(extension) => Self {
                picture_type: FRONT_COVER,
                mime_type: match extension {
                    "png" => "image/png",
                    "gif" => "image/gif",
                    "bmp" => "image/bmp",
                    _ => "image/jpeg",
                }
                .to_string(),
                description: format!("cover.{extension}"),
                data,
            },
            None => Self {
                picture_type: 0xFF,
                mime_type: "application/octet-stream".into(),
                description: String::new(),
                data,
            },
        }
    }

    fn from_lofty(picture: &Picture) -> Self {
        Self {
            picture_type: picture.pic_type().as_u8(),
            mime_type: picture
                .mime_type()
                .map(|mime| mime.as_str().to_string())
                .unwrap_or_default(),
            description: picture.description().unwrap_or_default().to_string(),
            data: picture.data().to_vec(),
        }
    }

    pub(crate) fn to_lofty(&self) -> Picture {
        let mut builder =
            Picture::unchecked(self.data.clone()).pic_type(PictureType::from_u8(self.picture_type));
        if !self.mime_type.is_empty() {
            builder = builder.mime_type(MimeType::from_str(&self.mime_type));
        }
        if !self.description.is_empty() {
            builder = builder.description(self.description.clone());
        }
        builder.build()
    }
}

enum Inner {
    Mpeg,
    Flac(Box<FlacFile>),
    Opus(Box<OpusFile>),
    Vorbis(Box<VorbisFile>),
    Speex(Box<SpeexFile>),
    Mp4(Box<Mp4File>),
    Aiff,
    Wav,
    Other,
}

/// An audio file's tags, open for reading and changing (`TagLib.File`).
pub struct TagFile {
    path: PathBuf,
    format: Format,
    inner: Inner,
    duration: Duration,
    sample_rate: u32,
    pub(crate) id3v2: Option<Id3Tag>,
    pub(crate) id3v1: Option<Id3v1Tag>,
    pub(crate) xiph: Option<XiphComment>,
    pub(crate) apple: Option<AppleTag>,
    /// A FLAC file's picture blocks, in file order.
    pub(crate) flac_pictures: Vec<(Picture, PictureInformation)>,
    /// `TagTypesOnDisk` has ID3v2: the file arrived with an ID3v2 tag.
    pub(crate) id3v2_on_disk: bool,
}

impl std::fmt::Debug for TagFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagFile")
            .field("path", &self.path)
            .field("format", &self.format)
            .finish()
    }
}

/// How TagLib# picks a reader: by the file name's extension.
fn format_of(path: &Path) -> Option<Format> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "mp3" | "mp2" | "mp1" | "mpa" => Format::Mpeg,
        "flac" => Format::Flac,
        "ogg" | "oga" | "opus" | "spx" => Format::Ogg,
        "m4a" | "m4b" | "m4p" | "m4r" | "m4v" | "mp4" | "3gp" => Format::Mp4,
        "aif" | "aiff" | "aifc" => Format::Aiff,
        "wav" | "wave" => Format::Wav,
        "aac" | "ape" | "wv" | "mpc" | "mp+" | "mpp" => Format::Other,
        _ => return None,
    })
}

/// Whether a FLAC file's metadata blocks (after an ID3v2 tag, if any) run to the end of the
/// file, that is, it has no audio frames.
fn flac_metadata_reaches_end(file: &mut std::fs::File) -> std::io::Result<bool> {
    use std::io::{Read, SeekFrom};
    let length = file.metadata()?.len();
    let mut header = [0u8; 10];
    file.rewind()?;
    file.read_exact(&mut header[..4])?;
    let mut position = 0u64;
    if &header[..3] == b"ID3" {
        file.read_exact(&mut header[4..10])?;
        let size = header[6..10]
            .iter()
            .fold(0u64, |size, byte| (size << 7) | u64::from(*byte));
        let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
        position = 10 + size + footer;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut header[..4])?;
    }
    if &header[..4] != b"fLaC" {
        return Ok(false);
    }
    position += 4;
    loop {
        let mut block = [0u8; 4];
        file.seek(SeekFrom::Start(position))?;
        if file.read_exact(&mut block).is_err() {
            return Ok(true);
        }
        let size = u64::from(block[1]) << 16 | u64::from(block[2]) << 8 | u64::from(block[3]);
        position += 4 + size;
        if block[0] & 0x80 != 0 || position >= length {
            file.rewind()?;
            return Ok(position >= length);
        }
    }
}

/// lofty reads what is on disk, unconverted: TagLib# reads a version 3 date its own way (see
/// `Id3Tag::from_lofty`), and keeps a Vorbis TRACKNUMBER and an MP4 gnre as they are.
fn parse_options(implicit_conversions: bool) -> ParseOptions {
    ParseOptions::new()
        .parsing_mode(ParsingMode::Relaxed)
        .read_cover_art(true)
        .implicit_conversions(implicit_conversions)
}

impl TagFile {
    /// `TagLib.File.Create(path)`.
    pub fn open(path: impl AsRef<Path>) -> Result<TagFile, TagError> {
        let path = path.as_ref().to_path_buf();
        let format = format_of(&path).ok_or_else(|| {
            TagError::Unsupported(path.extension().unwrap_or_default().to_string_lossy().into())
        })?;
        let mut reader = std::fs::File::open(&path)?;
        let mut file = TagFile {
            path,
            format,
            inner: Inner::Other,
            duration: Duration::ZERO,
            sample_rate: 0,
            id3v2: None,
            id3v1: None,
            xiph: None,
            apple: None,
            flac_pictures: Vec::new(),
            id3v2_on_disk: false,
        };
        match format {
            Format::Mpeg => {
                let mut mpeg = MpegFile::read_from(&mut reader, parse_options(false))?;
                file.set_properties(mpeg.properties().duration(), mpeg.properties().sample_rate());
                file.take_id3(mpeg.remove_id3v2());
                // TagLib# gives every MPEG file both ID3 tags.
                file.id3v1 = Some(mpeg.remove_id3v1().unwrap_or_default());
                file.inner = Inner::Mpeg;
            }
            Format::Flac => {
                let mut flac = FlacFile::read_from(&mut reader, parse_options(false))?;
                // TagLib#'s FLAC length needs audio after the metadata (`StreamHeader.Duration` is
                // zero for a stream length of zero), whatever STREAMINFO claims; lofty reads it
                // from STREAMINFO alone, and leaves the audio bitrate at zero for no audio.
                let duration = if flac.properties().audio_bitrate() == 0 {
                    Duration::ZERO
                } else {
                    flac.properties().duration()
                };
                file.set_properties(duration, flac.properties().sample_rate());
                let comments = flac.remove_vorbis_comments();
                file.xiph = Some(
                    comments
                        .as_ref()
                        .map_or_else(XiphComment::new, XiphComment::from_lofty),
                );
                file.flac_pictures = flac.remove_pictures();
                file.inner = Inner::Flac(Box::new(flac));
            }
            Format::Ogg => file.open_ogg(&mut reader)?,
            Format::Mp4 => {
                let mut mp4 = Mp4File::read_from(&mut reader, parse_options(false))?;
                file.set_properties(
                    mp4.properties().duration(),
                    mp4.properties().sample_rate().unwrap_or(0),
                );
                file.apple = Some(
                    mp4.remove_ilst()
                        .as_ref()
                        .map_or_else(AppleTag::default, AppleTag::from_lofty),
                );
                file.inner = Inner::Mp4(Box::new(mp4));
            }
            Format::Aiff => {
                let mut aiff = AiffFile::read_from(&mut reader, parse_options(false))?;
                file.set_properties(aiff.properties().duration(), aiff.properties().sample_rate());
                file.take_id3(aiff.remove_id3v2());
                file.inner = Inner::Aiff;
            }
            Format::Wav => {
                let mut wav = WavFile::read_from(&mut reader, parse_options(false))?;
                file.set_properties(wav.properties().duration(), wav.properties().sample_rate());
                file.take_id3(wav.remove_id3v2());
                file.inner = Inner::Wav;
            }
            Format::Other => {
                let tagged = lofty::read_from(&mut reader)?;
                file.set_properties(
                    tagged.properties().duration(),
                    tagged.properties().sample_rate().unwrap_or(0),
                );
            }
        }
        Ok(file)
    }

    fn set_properties(&mut self, duration: Duration, sample_rate: u32) {
        self.duration = duration;
        self.sample_rate = sample_rate;
    }

    fn take_id3(&mut self, tag: Option<Id3v2Tag>) {
        self.id3v2_on_disk = tag.is_some();
        self.id3v2 = Some(tag.map_or_else(Id3Tag::new, Id3Tag::from_lofty));
    }

    fn open_ogg(&mut self, reader: &mut std::fs::File) -> Result<(), TagError> {
        let probe = lofty::probe::Probe::new(&mut *reader)
            .options(parse_options(false))
            .guess_file_type()?;
        let file_type = probe.file_type();
        reader.rewind()?;
        let (comments, inner) = match file_type {
            Some(FileType::Opus) => {
                let opus = OpusFile::read_from(reader, parse_options(false))?;
                self.set_properties(
                    opus.properties().duration(),
                    opus.properties().input_sample_rate(),
                );
                (
                    XiphComment::from_lofty(opus.vorbis_comments()),
                    Inner::Opus(Box::new(opus)),
                )
            }
            Some(FileType::Vorbis) => {
                let vorbis = VorbisFile::read_from(reader, parse_options(false))?;
                self.set_properties(vorbis.properties().duration(), vorbis.properties().sample_rate());
                (
                    XiphComment::from_lofty(vorbis.vorbis_comments()),
                    Inner::Vorbis(Box::new(vorbis)),
                )
            }
            Some(FileType::Speex) => {
                let speex = SpeexFile::read_from(reader, parse_options(false))?;
                self.set_properties(speex.properties().duration(), speex.properties().sample_rate());
                (
                    XiphComment::from_lofty(speex.vorbis_comments()),
                    Inner::Speex(Box::new(speex)),
                )
            }
            other => return Err(TagError::Unsupported(format!("Ogg stream {other:?}"))),
        };
        self.xiph = Some(comments);
        self.inner = inner;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// `Properties.Duration`.
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// `(int)Math.Round(Properties.Duration.TotalSeconds)`.
    pub fn duration_seconds(&self) -> i32 {
        super::net::round_seconds(self.duration.as_secs_f64())
    }

    /// `Properties.AudioSampleRate`.
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }

    /// The file has an ID3v2 tag (`GetTag(TagTypes.Id3v2, false) is not null`).
    pub fn has_id3v2(&self) -> bool {
        self.id3v2.is_some()
    }

    /// `File.Save()`.
    pub fn save(&mut self) -> Result<(), TagError> {
        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let options = WriteOptions::new().lossy_text_encoding(true);
        match &mut self.inner {
            Inner::Mpeg | Inner::Aiff | Inner::Wav => {
                let tag_type = TagType::Id3v2;
                match self.id3v2.as_ref().filter(|tag| !tag.is_empty()) {
                    Some(tag) => {
                        let (tag, version3) = tag.to_lofty();
                        tag.save_to(&mut file, options.clone().use_id3v23(version3))?;
                    }
                    None => tag_type.remove_from(&mut file, options)?,
                }
                if matches!(self.inner, Inner::Mpeg) {
                    file.rewind()?;
                    match self.id3v1.as_ref().filter(|tag| !tag.is_empty()) {
                        Some(tag) => tag.save_to(&mut file, options)?,
                        None => TagType::Id3v1.remove_from(&mut file, options)?,
                    }
                }
            }
            Inner::Flac(flac) => {
                if let Some(xiph) = &mut self.xiph {
                    flac.set_vorbis_comments(xiph.render());
                }
                flac.remove_pictures();
                for (picture, info) in &self.flac_pictures {
                    flac.insert_picture(picture.clone(), Some(*info))?;
                }
                // lofty 0.25's splice truncates a file whose edited range runs to its end to the
                // size of what it removed instead of to the new length. A FLAC's metadata only
                // runs to its end when it has no audio, but then a shrinking comment would wreck
                // it, so a byte stands in for the audio while lofty writes.
                let stand_in = flac_metadata_reaches_end(&mut file)?;
                if stand_in {
                    file.seek(std::io::SeekFrom::End(0))?;
                    std::io::Write::write_all(&mut file, &[0])?;
                    file.rewind()?;
                }
                flac.save_to(&mut file, options)?;
                if stand_in {
                    let length = file.metadata()?.len();
                    file.set_len(length.saturating_sub(1))?;
                }
            }
            Inner::Opus(opus) => {
                if let Some(xiph) = &mut self.xiph {
                    opus.set_vorbis_comments(xiph.render());
                }
                opus.save_to(&mut file, options)?;
            }
            Inner::Vorbis(vorbis) => {
                if let Some(xiph) = &mut self.xiph {
                    vorbis.set_vorbis_comments(xiph.render());
                }
                vorbis.save_to(&mut file, options)?;
            }
            Inner::Speex(speex) => {
                if let Some(xiph) = &mut self.xiph {
                    speex.set_vorbis_comments(xiph.render());
                }
                speex.save_to(&mut file, options)?;
            }
            Inner::Mp4(mp4) => {
                if let Some(apple) = &self.apple {
                    mp4.set_ilst(apple.to_lofty());
                }
                mp4.save_to(&mut file, options)?;
            }
            Inner::Other => {
                return Err(TagError::Unsupported(format!(
                    "tags are not written to {}",
                    self.path.display()
                )));
            }
        }
        Ok(())
    }

    // ---- TagLib#'s generic Tag, over the tags this file has --------------------------------

    /// `Tag.Title`.
    pub fn title(&self) -> Option<String> {
        if let Some(id3) = &self.id3v2
            && let Some(title) = id3.text_string("TIT2")
        {
            return Some(title);
        }
        if let Some(v1) = &self.id3v1
            && let Some(title) = v1.title.as_ref().filter(|title| !title.is_empty())
        {
            return Some(title.clone());
        }
        if let Some(xiph) = &self.xiph {
            return xiph.first_field("TITLE");
        }
        self.apple
            .as_ref()
            .and_then(|apple| apple.text(names::NAM).into_iter().next())
    }

    pub fn set_title(&mut self, title: Option<&str>) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_text_frame("TIT2", &[title.unwrap_or_default()]);
        }
        if let Some(v1) = &mut self.id3v1 {
            v1.title = title
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty());
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("TITLE", &[title.unwrap_or_default()]);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_text(names::NAM, title);
        }
    }

    /// `Tag.Performers`.
    pub fn performers(&self) -> Vec<String> {
        if let Some(id3) = &self.id3v2 {
            let values = id3.text_values("TPE1");
            if !values.is_empty() {
                return values;
            }
        }
        if let Some(v1) = &self.id3v1
            && let Some(artist) = v1.artist.as_ref().filter(|artist| !artist.is_empty())
        {
            return artist.split(';').map(str::to_string).collect();
        }
        if let Some(xiph) = &self.xiph {
            return xiph.field("ARTIST");
        }
        self.apple
            .as_ref()
            .map(|apple| apple.text(names::ART))
            .unwrap_or_default()
    }

    /// `Tag.FirstPerformer`.
    pub fn first_performer(&self) -> Option<String> {
        self.performers().into_iter().next()
    }

    pub fn set_performers(&mut self, performers: &[String]) {
        let values: Vec<&str> = performers.iter().map(String::as_str).collect();
        if let Some(id3) = &mut self.id3v2 {
            id3.set_text_frame("TPE1", &values);
        }
        if let Some(v1) = &mut self.id3v1 {
            v1.artist = Some(values.join(";")).filter(|artist| !artist.is_empty());
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("ARTIST", &values);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_texts(names::ART, performers);
        }
    }

    /// `Tag.AlbumArtists`.
    pub fn album_artists(&self) -> Vec<String> {
        if let Some(id3) = &self.id3v2 {
            let values = id3.text_values("TPE2");
            if !values.is_empty() {
                return values;
            }
        }
        if let Some(xiph) = &self.xiph {
            return xiph.album_artists();
        }
        self.apple
            .as_ref()
            .map(|apple| apple.text(names::AART))
            .unwrap_or_default()
    }

    /// `Tag.FirstAlbumArtist`.
    pub fn first_album_artist(&self) -> Option<String> {
        self.album_artists().into_iter().next()
    }

    pub fn set_album_artists(&mut self, album_artists: &[String]) {
        let values: Vec<&str> = album_artists.iter().map(String::as_str).collect();
        if let Some(id3) = &mut self.id3v2 {
            id3.set_text_frame("TPE2", &values);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("ALBUMARTIST", &values);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_texts(names::AART, album_artists);
        }
    }

    /// `Tag.Composers`.
    pub fn composers(&self) -> Vec<String> {
        if let Some(id3) = &self.id3v2 {
            let values = id3.text_values("TCOM");
            if !values.is_empty() {
                return values;
            }
        }
        if let Some(xiph) = &self.xiph {
            return xiph.field("COMPOSER");
        }
        self.apple
            .as_ref()
            .map(|apple| apple.text(names::WRT))
            .unwrap_or_default()
    }

    pub fn set_composers(&mut self, composers: &[String]) {
        let values: Vec<&str> = composers.iter().map(String::as_str).collect();
        if let Some(id3) = &mut self.id3v2 {
            id3.set_text_frame("TCOM", &values);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("COMPOSER", &values);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_texts(names::WRT, composers);
        }
    }

    /// `Tag.Album`.
    pub fn album(&self) -> Option<String> {
        if let Some(id3) = &self.id3v2
            && let Some(album) = id3.text_string("TALB")
        {
            return Some(album);
        }
        if let Some(v1) = &self.id3v1
            && let Some(album) = v1.album.as_ref().filter(|album| !album.is_empty())
        {
            return Some(album.clone());
        }
        if let Some(xiph) = &self.xiph {
            return xiph.first_field("ALBUM");
        }
        self.apple
            .as_ref()
            .and_then(|apple| apple.text(names::ALB).into_iter().next())
    }

    pub fn set_album(&mut self, album: Option<&str>) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_text_frame("TALB", &[album.unwrap_or_default()]);
        }
        if let Some(v1) = &mut self.id3v1 {
            v1.album = album
                .map(|album| album.trim().to_string())
                .filter(|album| !album.is_empty());
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("ALBUM", &[album.unwrap_or_default()]);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_text(names::ALB, album);
        }
    }

    /// `Tag.Genres`: an ID3 genre that is a number reads as its name.
    pub fn genres(&self) -> Vec<String> {
        if let Some(id3) = &self.id3v2 {
            let values: Vec<String> = id3
                .text_values("TCON")
                .into_iter()
                .filter(|genre| !genre.is_empty())
                .map(|genre| genres::text_to_audio(&genre).map_or(genre, str::to_string))
                .collect();
            if !values.is_empty() {
                return values;
            }
        }
        if let Some(v1) = &self.id3v1
            && let Some(genre) = v1.genre.and_then(genres::index_to_audio)
        {
            return vec![genre.to_string()];
        }
        if let Some(xiph) = &self.xiph {
            return xiph.field("GENRE");
        }
        self.apple.as_ref().map(AppleTag::genres).unwrap_or_default()
    }

    /// `Tag.Genres` (set). ID3v2 writes a genre in TagLib#'s table as its number
    /// (`UseNumericGenres`); ID3v1 keeps the first genre's number.
    pub fn set_genres(&mut self, values: &[String]) {
        if let Some(id3) = &mut self.id3v2 {
            let numeric: Vec<String> = values
                .iter()
                .map(|genre| match genres::audio_to_index(genre) {
                    255 => genre.clone(),
                    index => index.to_string(),
                })
                .collect();
            let numeric: Vec<&str> = numeric.iter().map(String::as_str).collect();
            id3.set_text_frame("TCON", &numeric);
        }
        if let Some(v1) = &mut self.id3v1 {
            v1.genre = match values.first() {
                Some(first) => Some(genres::audio_to_index(first.trim())),
                None => Some(255),
            };
        }
        if let Some(xiph) = &mut self.xiph {
            let values: Vec<&str> = values.iter().map(String::as_str).collect();
            xiph.set_field("GENRE", &values);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_genres(values);
        }
    }

    /// `Tag.Year`.
    pub fn year(&self) -> u32 {
        if let Some(id3) = &self.id3v2 {
            let year = id3.text_string("TDRC").and_then(|text| {
                let head: String = text.chars().take(4).collect();
                (text.chars().count() >= 4).then(|| parse_uint(&head)).flatten()
            });
            if let Some(year @ 1..) = year {
                return year;
            }
        }
        if let Some(v1) = &self.id3v1
            && let Some(year @ 1..) = v1.year
        {
            return u32::from(year);
        }
        if let Some(xiph) = &self.xiph {
            return xiph.year();
        }
        self.apple.as_ref().map_or(0, AppleTag::year)
    }

    pub fn set_year(&mut self, year: u32) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_number_frame("TDRC", if year > 9999 { 0 } else { year }, 0, 1);
        }
        if let Some(v1) = &mut self.id3v1 {
            v1.year = (year > 0 && year < 10000).then_some(year as u16);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_number_field("DATE", year, 1);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_year(year);
        }
    }

    /// `Tag.Track`.
    pub fn track(&self) -> u32 {
        self.first_number(
            |id3| id3.text_number("TRCK", 0),
            |v1| v1.track_number.map_or(0, u32::from),
            |x| x.track(),
            AppleTag::track,
        )
    }

    /// `Tag.TrackCount`.
    pub fn track_count(&self) -> u32 {
        self.first_number(
            |id3| id3.text_number("TRCK", 1),
            |_| 0,
            XiphComment::track_count,
            AppleTag::track_count,
        )
    }

    /// `Tag.Disc`.
    pub fn disc(&self) -> u32 {
        self.first_number(
            |id3| id3.text_number("TPOS", 0),
            |_| 0,
            XiphComment::disc,
            AppleTag::disc,
        )
    }

    /// `Tag.DiscCount`.
    pub fn disc_count(&self) -> u32 {
        self.first_number(
            |id3| id3.text_number("TPOS", 1),
            |_| 0,
            XiphComment::disc_count,
            AppleTag::disc_count,
        )
    }

    /// A combined number: the first tag's that is not zero.
    fn first_number(
        &self,
        id3: impl Fn(&Id3Tag) -> u32,
        v1: impl Fn(&Id3v1Tag) -> u32,
        xiph: impl Fn(&XiphComment) -> u32,
        apple: impl Fn(&AppleTag) -> u32,
    ) -> u32 {
        [
            self.id3v2.as_ref().map(&id3),
            self.id3v1.as_ref().map(&v1),
            self.xiph.as_ref().map(&xiph),
            self.apple.as_ref().map(&apple),
        ]
        .into_iter()
        .flatten()
        .find(|value| *value != 0)
        .unwrap_or(0)
    }

    pub fn set_track(&mut self, track: u32) {
        let count = self.id3v2.as_ref().map(|id3| id3.text_number("TRCK", 1));
        if let (Some(id3), Some(count)) = (&mut self.id3v2, count) {
            id3.set_number_frame("TRCK", track, count, 2);
        }
        if let Some(v1) = &mut self.id3v1 {
            v1.track_number = Some(if track < 256 { track as u8 } else { 0 }).filter(|track| *track != 0);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_track(track);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_track(track);
        }
    }

    pub fn set_track_count(&mut self, count: u32) {
        let track = self.id3v2.as_ref().map(|id3| id3.text_number("TRCK", 0));
        if let (Some(id3), Some(track)) = (&mut self.id3v2, track) {
            id3.set_number_frame("TRCK", track, count, 2);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_track_count(count);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_track_count(count);
        }
    }

    pub fn set_disc(&mut self, disc: u32) {
        let count = self.id3v2.as_ref().map(|id3| id3.text_number("TPOS", 1));
        if let (Some(id3), Some(count)) = (&mut self.id3v2, count) {
            id3.set_number_frame("TPOS", disc, count, 1);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_disc(disc);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_disc(disc);
        }
    }

    pub fn set_disc_count(&mut self, count: u32) {
        let disc = self.id3v2.as_ref().map(|id3| id3.text_number("TPOS", 0));
        if let (Some(id3), Some(disc)) = (&mut self.id3v2, disc) {
            id3.set_number_frame("TPOS", disc, count, 1);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_disc_count(count);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_disc_count(count);
        }
    }

    /// `Tag.BeatsPerMinute`.
    pub fn bpm(&mut self) -> u32 {
        if let Some(id3) = &self.id3v2 {
            let bpm = id3
                .text_string("TBPM")
                .and_then(|text| parse_double(&text))
                .filter(|bpm| *bpm >= 0.0);
            if let Some(bpm) = bpm
                .map(|bpm| bpm.round_ties_even() as u32)
                .filter(|bpm| *bpm != 0)
            {
                return bpm;
            }
        }
        if let Some(xiph) = &mut self.xiph {
            let bpm = xiph.bpm();
            if bpm != 0 {
                return bpm;
            }
        }
        self.apple.as_ref().map_or(0, AppleTag::bpm)
    }

    pub fn set_bpm(&mut self, bpm: u32) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_number_frame("TBPM", bpm, 0, 1);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_bpm(bpm);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_bpm(bpm);
        }
    }

    /// `Tag.Copyright`.
    pub fn copyright(&self) -> Option<String> {
        self.first_string(
            |id3| id3.text_string("TCOP"),
            "COPYRIGHT",
            |apple| apple.first_text(names::CPRT),
        )
    }

    pub fn set_copyright(&mut self, copyright: Option<&str>) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_text_frame("TCOP", &[copyright.unwrap_or_default()]);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("COPYRIGHT", &[copyright.unwrap_or_default()]);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_text(names::CPRT, copyright);
        }
    }

    /// A combined string with no ID3v1 part: the first tag's that is not null.
    fn first_string(
        &self,
        id3: impl Fn(&Id3Tag) -> Option<String>,
        xiph_key: &str,
        apple: impl Fn(&AppleTag) -> Option<String>,
    ) -> Option<String> {
        if let Some(value) = self.id3v2.as_ref().and_then(&id3) {
            return Some(value);
        }
        if let Some(xiph) = &self.xiph {
            return xiph.first_field(xiph_key);
        }
        self.apple.as_ref().and_then(apple)
    }

    /// `Tag.Lyrics`.
    pub fn lyrics(&self) -> Option<String> {
        self.first_string(Id3Tag::lyrics, "LYRICS", |apple| apple.first_text(names::LYR))
    }

    pub fn set_lyrics(&mut self, lyrics: Option<&str>) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_lyrics(lyrics);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field("LYRICS", &[lyrics.unwrap_or_default()]);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_text(names::LYR, lyrics);
        }
    }

    /// `Tag.ISRC`. An Ogg file's tag (`GroupedComment`) does not have one.
    pub fn isrc(&self) -> Option<String> {
        if self.format == Format::Ogg {
            return None;
        }
        self.first_string(
            |id3| id3.text_string("TSRC"),
            "ISRC",
            |apple| apple.dash_box(super::tag_writer_extras::APPLE_MEAN, "ISRC"),
        )
    }

    /// `Tag.Publisher`. An Ogg file's tag (`GroupedComment`) does not have one.
    pub fn publisher(&self) -> Option<String> {
        if self.format == Format::Ogg {
            return None;
        }
        self.first_string(
            |id3| id3.text_string("TPUB"),
            "ORGANIZATION",
            |apple| apple.dash_box(super::tag_writer_extras::APPLE_MEAN, "publisher"),
        )
    }

    /// `Tag.MusicBrainzReleaseId`.
    pub fn music_brainz_release_id(&self) -> Option<String> {
        self.first_string(
            |id3| id3.user_text_string("MusicBrainz Album Id", false),
            "MUSICBRAINZ_ALBUMID",
            |apple| apple.dash_box(super::tag_writer_extras::APPLE_MEAN, "MusicBrainz Album Id"),
        )
    }

    pub fn set_music_brainz_release_id(&mut self, id: Option<&str>) {
        self.set_music_brainz("MusicBrainz Album Id", "MUSICBRAINZ_ALBUMID", id);
    }

    /// `Tag.MusicBrainzReleaseGroupId`.
    pub fn music_brainz_release_group_id(&self) -> Option<String> {
        self.first_string(
            |id3| id3.user_text_string("MusicBrainz Release Group Id", false),
            "MUSICBRAINZ_RELEASEGROUPID",
            |apple| {
                apple.dash_box(
                    super::tag_writer_extras::APPLE_MEAN,
                    "MusicBrainz Release Group Id",
                )
            },
        )
    }

    pub fn set_music_brainz_release_group_id(&mut self, id: Option<&str>) {
        self.set_music_brainz("MusicBrainz Release Group Id", "MUSICBRAINZ_RELEASEGROUPID", id);
    }

    fn set_music_brainz(&mut self, name: &str, vorbis: &str, id: Option<&str>) {
        if let Some(id3) = &mut self.id3v2 {
            id3.set_user_text_string(name, id);
        }
        if let Some(xiph) = &mut self.xiph {
            xiph.set_field(vorbis, &[id.unwrap_or_default()]);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_dash_box(super::tag_writer_extras::APPLE_MEAN, name, id);
        }
    }

    /// `Tag.Pictures`.
    pub fn pictures(&self) -> Vec<TagPicture> {
        if let Some(id3) = &self.id3v2 {
            let pictures: Vec<TagPicture> = id3
                .pictures()
                .iter()
                .map(|picture| TagPicture::from_lofty(&picture.picture))
                .collect();
            if !pictures.is_empty() {
                return pictures;
            }
        }
        if self.format == Format::Flac {
            return self
                .flac_pictures
                .iter()
                .map(|(picture, _)| TagPicture::from_lofty(picture))
                .collect();
        }
        if let Some(xiph) = &self.xiph {
            return xiph
                .pictures()
                .iter()
                .map(|(picture, _)| TagPicture::from_lofty(picture))
                .collect();
        }
        self.apple
            .as_ref()
            .map(|apple| apple.pictures().into_iter().map(TagPicture::from_data).collect())
            .unwrap_or_default()
    }

    /// `Tag.Pictures` (set). A FLAC file keeps them in its own picture blocks.
    pub fn set_pictures(&mut self, pictures: &[TagPicture]) {
        let lofty: Vec<Picture> = pictures.iter().map(TagPicture::to_lofty).collect();
        if let Some(id3) = &mut self.id3v2 {
            id3.set_pictures(&lofty);
        }
        if self.format == Format::Flac {
            self.flac_pictures = lofty
                .iter()
                .map(|picture| (picture.clone(), PictureInformation::default()))
                .collect();
        } else if let Some(xiph) = &mut self.xiph {
            xiph.set_pictures(&lofty);
        }
        if let Some(apple) = &mut self.apple {
            apple.set_pictures(&lofty);
        }
    }
}
