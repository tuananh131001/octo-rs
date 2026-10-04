//! Port of `Services/Common/TagWriterExtras.cs`: the tag frames TagLib's generic Tag does not
//! expose, written in the frame each container's readers look for. Navidrome's mappings.yaml
//! maps TXXX:ARTISTS (ID3v2), ARTISTS (Vorbis) and ----:com.apple.iTunes:ARTISTS (MP4) to its
//! artists tag, and reads the recording id from UFID:http://musicbrainz.org,
//! MUSICBRAINZ_TRACKID and "MusicBrainz Track Id".

use std::path::Path;

use octo_core::common::dotnet::is_null_or_white_space;
use octo_core::common::song_identity::SongIdentity;
use octo_core::tagging::{FileFacts, FileFactsReader};

use super::apple::AppleTag;
use super::id3::Id3Tag;
use super::tag_file::{Format, TagFile};
use super::xiph::XiphComment;
use crate::audio::net_format;

/// Picard's and Navidrome's owner string for the recording id on ID3.
pub const MUSIC_BRAINZ_UFID_OWNER: &str = "http://musicbrainz.org";

/// The mean of the iTunes freeform atoms.
pub const APPLE_MEAN: &str = "com.apple.iTunes";

/// One field as each container names it, in the names the common taggers write and the
/// library server reads. A null ID3 frame means the field lives in a TXXX frame with the
/// description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagField {
    pub id3_frame: Option<&'static str>,
    pub id3_description: Option<&'static str>,
    pub vorbis: &'static str,
    pub mp4: &'static str,
}

const fn txxx(description: &'static str, vorbis: &'static str, mp4: &'static str) -> TagField {
    TagField {
        id3_frame: None,
        id3_description: Some(description),
        vorbis,
        mp4,
    }
}

/// The fields Octo writes beyond what TagLib's generic tag exposes, named per container.
pub struct TagFields;

impl TagFields {
    pub const ISRC: TagField = TagField {
        id3_frame: Some("TSRC"),
        id3_description: None,
        vorbis: "ISRC",
        mp4: "ISRC",
    };
    pub const LABEL: TagField = TagField {
        id3_frame: Some("TPUB"),
        id3_description: None,
        vorbis: "LABEL",
        mp4: "LABEL",
    };
    pub const CATALOG_NUMBER: TagField = txxx("CATALOGNUMBER", "CATALOGNUMBER", "CATALOGNUMBER");
    pub const BARCODE: TagField = txxx("BARCODE", "BARCODE", "BARCODE");
    pub const RELEASE_TYPE: TagField =
        txxx("MusicBrainz Album Type", "RELEASETYPE", "MusicBrainz Album Type");
    pub const RELEASE_STATUS: TagField = txxx(
        "MusicBrainz Album Status",
        "RELEASESTATUS",
        "MusicBrainz Album Status",
    );
    pub const RELEASE_COUNTRY: TagField = txxx(
        "MusicBrainz Album Release Country",
        "RELEASECOUNTRY",
        "MusicBrainz Album Release Country",
    );
    pub const RELEASE_TRACK_ID: TagField = txxx(
        "MusicBrainz Release Track Id",
        "MUSICBRAINZ_RELEASETRACKID",
        "MusicBrainz Release Track Id",
    );
    pub const ALBUM_ARTIST_ID: TagField = txxx(
        "MusicBrainz Album Artist Id",
        "MUSICBRAINZ_ALBUMARTISTID",
        "MusicBrainz Album Artist Id",
    );
    pub const ARTIST_ID: TagField = txxx(
        "MusicBrainz Artist Id",
        "MUSICBRAINZ_ARTISTID",
        "MusicBrainz Artist Id",
    );
    pub const FINGERPRINT_ID: TagField = txxx("Acoustid Id", "ACOUSTID_ID", "Acoustid Id");
    pub const TRACK_GAIN: TagField = txxx(
        "REPLAYGAIN_TRACK_GAIN",
        "REPLAYGAIN_TRACK_GAIN",
        "REPLAYGAIN_TRACK_GAIN",
    );
    pub const TRACK_PEAK: TagField = txxx(
        "REPLAYGAIN_TRACK_PEAK",
        "REPLAYGAIN_TRACK_PEAK",
        "REPLAYGAIN_TRACK_PEAK",
    );
    pub const ALBUM_GAIN: TagField = txxx(
        "REPLAYGAIN_ALBUM_GAIN",
        "REPLAYGAIN_ALBUM_GAIN",
        "REPLAYGAIN_ALBUM_GAIN",
    );
    pub const ALBUM_PEAK: TagField = txxx(
        "REPLAYGAIN_ALBUM_PEAK",
        "REPLAYGAIN_ALBUM_PEAK",
        "REPLAYGAIN_ALBUM_PEAK",
    );
    pub const ALBUM_ID: TagField = txxx(
        "MusicBrainz Album Id",
        "MUSICBRAINZ_ALBUMID",
        "MusicBrainz Album Id",
    );
    pub const ALBUM_ARTISTS: TagField = txxx("ALBUMARTISTS", "ALBUMARTISTS", "ALBUMARTISTS");
    pub const ALBUM_VERSION: TagField = txxx("ALBUMVERSION", "ALBUMVERSION", "ALBUMVERSION");
    pub const RELEASE_DATE: TagField = txxx("RELEASEDATE", "RELEASEDATE", "RELEASEDATE");
}

/// The containers' own tags a setter writes to.
pub(crate) struct NativeTags<'a> {
    pub(crate) id3: Option<&'a mut Id3Tag>,
    pub(crate) xiph: Option<&'a mut XiphComment>,
    pub(crate) apple: Option<&'a mut AppleTag>,
}

/// `Id3For`: the file's ID3v2 tag. A tag Octo creates is version 4, so the original date has
/// its own frame; a tag the file arrived with keeps whatever version it has. Never a global
/// switch: that would change every other writer in Octo unseen.
pub(crate) fn id3_for(file: &mut TagFile) -> Option<&mut Id3Tag> {
    let on_disk = file.id3v2_on_disk;
    let id3 = file.id3v2.as_mut()?;
    if !on_disk {
        id3.version = 4;
    }
    Some(id3)
}

/// `NativeTags`: the container's own tag, created when the format has an obvious one. For
/// anything else only a tag that already exists is used, so a WAV never grows a Vorbis comment
/// it cannot hold.
pub(crate) fn native_tags(file: &mut TagFile) -> NativeTags<'_> {
    match file.format() {
        Format::Mpeg => NativeTags {
            id3: id3_for(file),
            xiph: None,
            apple: None,
        },
        Format::Mp4 => NativeTags {
            id3: None,
            xiph: None,
            apple: file.apple.as_mut(),
        },
        Format::Flac | Format::Ogg => NativeTags {
            id3: None,
            xiph: file.xiph.as_mut(),
            apple: None,
        },
        _ => NativeTags {
            id3: file.id3v2.as_mut(),
            xiph: file.xiph.as_mut(),
            apple: file.apple.as_mut(),
        },
    }
}

/// `string.Trim()` of a value that may be absent; None when nothing is left.
fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// One field holding several values, each its own value rather than one joined string.
/// TagLib writes a multi-value TXXX null-separated even in ID3v2.3, which is what keeps a
/// credit of two artists from being read back as one artist named after both.
pub fn set_multi_value(file: &mut TagFile, field: &str, values: &[String]) {
    let array: Vec<String> = values
        .iter()
        .filter(|value| !is_null_or_white_space(Some(value)))
        .map(|value| value.trim().to_string())
        .collect();
    if array.is_empty() {
        return;
    }
    let tags = native_tags(file);
    if let Some(id3) = tags.id3 {
        id3.set_user_text(field, &array, true);
    }
    if let Some(xiph) = tags.xiph {
        xiph.set_field(field, &as_strs(&array));
    }
    if let Some(apple) = tags.apple {
        apple.set_dash_boxes(APPLE_MEAN, field, &array);
    }
}

/// One value in one field, in the frame each container's readers look for. An empty value
/// leaves the file as it is: a release fact that could not be found is not a reason to strip
/// one a peer wrote.
pub fn set_text(file: &mut TagFile, field: TagField, value: Option<&str>) {
    let Some(text) = trimmed(value) else {
        return;
    };
    let tags = native_tags(file);
    if let Some(id3) = tags.id3 {
        if let Some(frame) = field.id3_frame {
            id3.set_text_frame(frame, &[text]);
        } else if let Some(description) = field.id3_description {
            id3.set_user_text(description, &[text.to_string()], true);
        }
    }
    if let Some(xiph) = tags.xiph {
        xiph.set_field(field.vorbis, &[text]);
    }
    if let Some(apple) = tags.apple {
        apple.set_dash_box(APPLE_MEAN, field.mp4, Some(text));
    }
}

/// Several values in one field, by the same names as [`set_text`].
pub fn set_multi(file: &mut TagFile, field: TagField, values: &[String]) {
    let mut array: Vec<String> = Vec::new();
    for value in values.iter().filter(|value| !is_null_or_white_space(Some(value))) {
        let value = value.trim().to_string();
        if !array.contains(&value) {
            array.push(value);
        }
    }
    if array.is_empty() {
        return;
    }
    let tags = native_tags(file);
    if let Some(id3) = tags.id3 {
        if let Some(frame) = field.id3_frame {
            id3.set_text_frame(frame, &as_strs(&array));
        } else if let Some(description) = field.id3_description {
            id3.set_user_text(description, &array, true);
        }
    }
    if let Some(xiph) = tags.xiph {
        xiph.set_field(field.vorbis, &as_strs(&array));
    }
    if let Some(apple) = tags.apple {
        apple.set_dash_boxes(APPLE_MEAN, field.mp4, &array);
    }
}

/// Values exactly as given, untrimmed, replacing the field; none removes it. For copying
/// another file's values, where one changed character changes what they hash to.
pub fn set_exact(file: &mut TagFile, field: TagField, values: &[String]) {
    let array: Vec<String> = values.iter().filter(|value| !value.is_empty()).cloned().collect();
    let tags = native_tags(file);
    if let (Some(id3), Some(description)) = (tags.id3, field.id3_description) {
        if !array.is_empty() {
            id3.set_user_text(description, &array, true);
        } else {
            id3.remove_user_text(description, true);
        }
    }
    if let Some(xiph) = tags.xiph {
        if !array.is_empty() {
            xiph.set_field(field.vorbis, &as_strs(&array));
        } else {
            xiph.remove_field(field.vorbis);
        }
    }
    if let Some(apple) = tags.apple {
        // TagLib's SetDashBoxes reads the first value before anything else, so none goes
        // through SetDashBox.
        if !array.is_empty() {
            apple.set_dash_boxes(APPLE_MEAN, field.mp4, &array);
        } else if apple.dash_box(APPLE_MEAN, field.mp4).is_some() {
            apple.set_dash_box(APPLE_MEAN, field.mp4, None);
        }
    }
}

/// The original release date: a TDOR frame on a version 4 tag, TORY (the year) on version 3,
/// ORIGINALDATE and ORIGINALYEAR on a Vorbis comment, originaldate on MP4. TagLib keeps every
/// frame under its version 4 id and renders TDOR as TORY on a version 3 tag, so the frame is
/// set as TDOR either way, with the year alone where TORY holds no more.
pub fn set_original_date(file: &mut TagFile, date: Option<&str>) {
    let Some(text) = trimmed(date) else {
        return;
    };
    let year: String = if text.chars().count() >= 4 {
        text.chars().take(4).collect()
    } else {
        text.to_string()
    };
    let tags = native_tags(file);
    if let Some(id3) = tags.id3 {
        let value = if id3.version >= 4 { text } else { year.as_str() };
        id3.set_text_frame("TDOR", &[value]);
    }
    if let Some(xiph) = tags.xiph {
        xiph.set_field("ORIGINALDATE", &[text]);
        xiph.set_field("ORIGINALYEAR", &[&year]);
    }
    if let Some(apple) = tags.apple {
        apple.set_dash_box(APPLE_MEAN, "originaldate", Some(text));
    }
}

/// The release track id, in the frame taggers and the library server read it from.
pub fn set_release_track_id(file: &mut TagFile, id: Option<&str>) {
    set_text(file, TagFields::RELEASE_TRACK_ID, id);
}

/// ReplayGain as the players read it: the gain with two decimals and " dB", the peak with six,
/// always with a dot, whatever the server's own culture.
pub fn set_replay_gain(
    file: &mut TagFile,
    track_gain_db: Option<f64>,
    track_peak: Option<f64>,
    album_gain_db: Option<f64>,
    album_peak: Option<f64>,
) {
    if let Some(gain) = track_gain_db {
        set_text(file, TagFields::TRACK_GAIN, Some(&gain_text(gain)));
    }
    if let Some(peak) = track_peak {
        set_text(file, TagFields::TRACK_PEAK, Some(&peak_text(peak)));
    }
    if let Some(gain) = album_gain_db {
        set_text(file, TagFields::ALBUM_GAIN, Some(&gain_text(gain)));
    }
    if let Some(peak) = album_peak {
        set_text(file, TagFields::ALBUM_PEAK, Some(&peak_text(peak)));
    }
}

/// `gainDb.ToString("+0.00;-0.00", CultureInfo.InvariantCulture) + " dB"`: a negative value
/// that rounds to zero takes the first section.
pub fn gain_text(gain_db: f64) -> String {
    let body = net_format::fixed(gain_db.abs(), 2, 2);
    let sign = if gain_db < 0.0 && body != "0.00" { '-' } else { '+' };
    format!("{sign}{body} dB")
}

/// `peak.ToString("0.000000", CultureInfo.InvariantCulture)`.
pub fn peak_text(peak: f64) -> String {
    net_format::fixed(peak, 6, 6)
}

/// One value as the file holds it, from whichever container's frame has it.
pub fn read_text(file: &TagFile, field: TagField) -> Option<String> {
    let mut from_id3 = None;
    if let Some(id3) = &file.id3v2 {
        if let Some(frame) = field.id3_frame {
            from_id3 = id3.text_values(frame).into_iter().next();
        } else if let Some(description) = field.id3_description {
            from_id3 = id3
                .user_text(description, true)
                .and_then(|values| values.into_iter().next());
        }
    }
    let value = from_id3
        .or_else(|| file.xiph.as_ref().and_then(|xiph| xiph.first_field(field.vorbis)))
        .or_else(|| {
            file.apple
                .as_ref()
                .and_then(|apple| apple.dash_box(APPLE_MEAN, field.mp4))
        });
    value
        .filter(|value| !is_null_or_white_space(Some(value)))
        .map(|value| value.trim().to_string())
}

/// The RECORDING id, in the frame Picard and Navidrome both read it from. Written frame by
/// frame rather than through Tag.MusicBrainzTrackId: TagLib# after 2.3.0 repurposes that
/// property for the release TRACK id on ID3 (UFID owner "MusicBrainz Release Track Id"), which
/// Navidrome would not read as a recording, and a package bump must not change what lands on
/// disk.
pub fn set_recording_id(file: &mut TagFile, recording_id: &str) {
    if is_null_or_white_space(Some(recording_id)) {
        return;
    }
    let tags = native_tags(file);
    if let Some(id3) = tags.id3 {
        id3.set_ufid(MUSIC_BRAINZ_UFID_OWNER, recording_id.as_bytes());
    }
    if let Some(xiph) = tags.xiph {
        xiph.set_field("MUSICBRAINZ_TRACKID", &[recording_id]);
    }
    if let Some(apple) = tags.apple {
        apple.set_dash_box(APPLE_MEAN, "MusicBrainz Track Id", Some(recording_id));
    }
}

/// The recording id a file already carries, from whichever frame holds it.
pub fn read_recording_id(file: &mut TagFile) -> Option<String> {
    let tags = native_tags(file);
    let from_id3 = tags.id3.and_then(|id3| {
        id3.ufid(MUSIC_BRAINZ_UFID_OWNER)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    });
    let value = from_id3
        .or_else(|| tags.xiph.and_then(|xiph| xiph.first_field("MUSICBRAINZ_TRACKID")))
        .or_else(|| {
            tags.apple
                .and_then(|apple| apple.dash_box(APPLE_MEAN, "MusicBrainz Track Id"))
        });
    value
        .filter(|value| !is_null_or_white_space(Some(value)))
        .map(|value| value.trim().to_string())
}

pub fn set_compilation(file: &mut TagFile, value: bool) {
    let tags = native_tags(file);
    if let Some(id3) = tags.id3 {
        id3.set_compilation(value);
    }
    if let Some(xiph) = tags.xiph {
        xiph.set_compilation(value);
    }
    if let Some(apple) = tags.apple {
        apple.set_compilation(value);
    }
}

pub fn is_compilation(file: &TagFile) -> bool {
    file.id3v2.as_ref().is_some_and(Id3Tag::is_compilation)
        || file.xiph.as_ref().is_some_and(XiphComment::is_compilation)
        || file.apple.as_ref().is_some_and(AppleTag::is_compilation)
}

/// The frame names a barcode is kept under, in the order they are asked.
const BARCODE_NAMES: [&str; 3] = ["BARCODE", "UPC", "EAN"];

/// `CoverUpgradeWorker.BarcodeOf` (`Services/CoverArt/CoverUpgrade.cs`), here because it reads
/// the containers' own tags: the album's barcode when the song carries one (about one song in
/// ten of Brandon's), from the Vorbis comment, then the ID3v2 TXXX frames in frame order, then
/// the iTunes freeform boxes. With it, the album needs no lookup to be matched at Apple.
pub fn barcode_of(file: &TagFile) -> Option<String> {
    let usable = |value: &String| !value.is_empty();
    if let Some(xiph) = &file.xiph {
        for name in BARCODE_NAMES {
            if let Some(value) = xiph.first_field(name).filter(usable) {
                return Some(value.trim().to_string());
            }
        }
    }
    if let Some(id3) = &file.id3v2 {
        for (description, values) in id3.user_texts() {
            if BARCODE_NAMES
                .iter()
                .any(|name| octo_core::common::dotnet::eq_ignore_case(name, &description))
                && let Some(value) = values.first().filter(|value| usable(value))
            {
                return Some(value.trim().to_string());
            }
        }
    }
    if let Some(apple) = &file.apple {
        for name in BARCODE_NAMES {
            if let Some(value) = apple.dash_box(APPLE_MEAN, name).filter(usable) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// The album a file already names, for a download whose source named none.
pub fn read_album(path: &Path) -> (Option<String>, Option<String>, bool) {
    match TagFile::open(path) {
        Ok(file) => (file.album(), file.first_album_artist(), is_compilation(&file)),
        Err(_) => (None, None, false),
    }
}

/// The recording id and length of a file on disk, for deciding whether two files are the same
/// recording. Nulls when the file cannot be read.
pub fn read_identity(path: &Path) -> (Option<String>, i32) {
    match TagFile::open(path) {
        Ok(mut file) => (read_recording_id(&mut file), file.duration_seconds()),
        Err(_) => (None, 0),
    }
}

/// What a file already says about itself, for the chooser: its length and format, and the
/// tags it arrived with. With `tags_are_evidence` off only the length and format are read,
/// since an uploader's name is not a credit.
pub fn read_facts(path: &str, tags_are_evidence: bool) -> FileFacts {
    let extension = net_format::get_extension(path).to_string();
    let Ok(mut file) = TagFile::open(path) else {
        return FileFacts::unknown(&extension);
    };
    let seconds = file.duration_seconds();
    let rate = file.sample_rate();
    if !tags_are_evidence {
        return FileFacts {
            duration_seconds: seconds,
            extension,
            sample_rate: rate,
            ..Default::default()
        };
    }

    let mut isrcs: Vec<Option<String>> = vec![file.isrc(), read_text(&file, TagFields::ISRC)];
    if let Some(values) = file.id3v2.as_ref().and_then(|id3| id3.user_text("ISRC", true)) {
        isrcs.extend(values.into_iter().map(Some));
    }
    let mut codes: Vec<String> = Vec::new();
    for value in isrcs
        .into_iter()
        .flatten()
        .filter(|value| !is_null_or_white_space(Some(value)))
    {
        for part in value.split([';', ',', '/', '\0']).filter(|part| !part.is_empty()) {
            if let Some(code) = SongIdentity::normalize_isrc(part)
                && !codes.contains(&code)
            {
                codes.push(code);
            }
        }
    }

    let positive = |value: u32| (value > 0).then_some(value as i32);
    FileFacts {
        duration_seconds: seconds,
        extension,
        sample_rate: rate,
        title: blank(file.title()),
        artist: blank(file.first_performer()),
        album: blank(file.album()),
        album_artist: blank(file.first_album_artist()),
        year: positive(file.year()),
        track: positive(file.track()),
        disc: positive(file.disc()),
        isrcs: codes,
        barcode: read_text(&file, TagFields::BARCODE),
        catalog_number: read_text(&file, TagFields::CATALOG_NUMBER),
        label: blank(file.publisher()).or_else(|| read_text(&file, TagFields::LABEL)),
        recording_id: read_recording_id(&mut file),
        release_id: blank(file.music_brainz_release_id()),
        is_compilation: is_compilation(&file),
        tags_are_evidence: true,
    }
}

fn blank(value: Option<String>) -> Option<String> {
    value
        .filter(|value| !is_null_or_white_space(Some(value)))
        .map(|value| value.trim().to_string())
}

fn as_strs(values: &[String]) -> Vec<&str> {
    values.iter().map(String::as_str).collect()
}

/// `TagWriterExtras.ReadFacts` as the release identifier asks for it.
#[derive(Debug, Clone, Copy, Default)]
pub struct TagWriterExtras;

impl FileFactsReader for TagWriterExtras {
    fn read_facts(&self, path: &str, tags_are_evidence: bool) -> FileFacts {
        read_facts(path, tags_are_evidence)
    }
}
