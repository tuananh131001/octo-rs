//! The Rust half of the tag-dump check: every frame, field and atom of a file as this port
//! reads it back, then Octo's own readers' views, in exactly the text the C# generator's
//! `Dumper` writes (docs/rust-migration/fixtures/tags/generator/Program.cs).

use std::fmt::Write as _;
use std::path::Path;

use lofty::id3::v2::Frame;
use lofty::picture::{Picture, PictureInformation};
use sha2::{Digest, Sha256};

use super::apple::{AppleBox, DataBox};
use super::kept_identity::KeptIdentityTags;
use super::tag_file::{Format, TagFile};
use super::tag_writer_extras::{self as extras, TagField, TagFields};
use super::xiph::PICTURE_FIELD;

const FIELDS: [(&str, TagField); 19] = [
    ("isrc", TagFields::ISRC),
    ("label", TagFields::LABEL),
    ("catalognumber", TagFields::CATALOG_NUMBER),
    ("barcode", TagFields::BARCODE),
    ("releasetype", TagFields::RELEASE_TYPE),
    ("releasestatus", TagFields::RELEASE_STATUS),
    ("releasecountry", TagFields::RELEASE_COUNTRY),
    ("releasetrackid", TagFields::RELEASE_TRACK_ID),
    ("albumartistid", TagFields::ALBUM_ARTIST_ID),
    ("artistid", TagFields::ARTIST_ID),
    ("acoustid", TagFields::FINGERPRINT_ID),
    ("trackgain", TagFields::TRACK_GAIN),
    ("trackpeak", TagFields::TRACK_PEAK),
    ("albumgain", TagFields::ALBUM_GAIN),
    ("albumpeak", TagFields::ALBUM_PEAK),
    ("albumid", TagFields::ALBUM_ID),
    ("albumartists", TagFields::ALBUM_ARTISTS),
    ("albumversion", TagFields::ALBUM_VERSION),
    ("releasedate", TagFields::RELEASE_DATE),
];

/// The dump of one file, line for line as the C# `Dumper.Dump` writes it.
pub(crate) fn dump(path: &Path) -> String {
    let mut o = String::new();
    let mut line = |text: String| {
        o.push_str(&text);
        o.push('\n');
    };
    {
        let mut file = TagFile::open(path).expect("the file opens");
        line(format!(
            "duration {} rate {}",
            file.duration_seconds(),
            file.sample_rate()
        ));
        if let Some(id3) = &file.id3v2 {
            line(format!("id3v2 version {}", id3.version));
            for frame in &id3.frames {
                line(format!("  {}", frame_line(frame)));
            }
        }
        if let Some(v1) = &file.id3v1 {
            let title = v1.title.clone().filter(|t| !t.is_empty());
            let performers: Vec<String> = v1
                .artist
                .as_ref()
                .filter(|a| !a.is_empty())
                .map(|a| a.split(';').map(str::to_string).collect())
                .unwrap_or_default();
            let joined = (!performers.is_empty()).then(|| performers.join("; "));
            let genre = v1
                .genre
                .and_then(super::genres::index_to_audio)
                .map(str::to_string);
            line(format!(
                "id3v1 title {} artist {} album {} year {} comment {} track {} genre {}",
                q(title.as_deref()),
                q(joined.as_deref()),
                q(v1.album.as_deref().filter(|a| !a.is_empty())),
                v1.year.unwrap_or(0),
                q(v1.comment.as_deref().filter(|c| !c.is_empty())),
                v1.track_number.unwrap_or(0),
                q(genre.as_deref())
            ));
        }
        if let Some(xiph) = &file.xiph {
            line(format!("xiph vendor {}", q(Some(&xiph.vendor))));
            for (key, values) in xiph.fields.iter() {
                let items: Vec<String> = if key == PICTURE_FIELD || key == "COVERART" {
                    values.iter().map(|value| block_picture(value)).collect()
                } else {
                    values.iter().map(|value| q(Some(value))).collect()
                };
                line(format!("  {key} {}", list(&items)));
            }
        }
        if let Some(apple) = &file.apple {
            line("apple".into());
            for apple_box in &apple.boxes {
                line(format!("  {}", atom_line(apple_box)));
            }
        }
        if file.format() == Format::Flac {
            for (picture, info) in &file.flac_pictures {
                line(format!("flacpicture {}", flac_picture(picture, info)));
            }
        }

        let year = file.year();
        let bpm = file.bpm();
        line("tag".into());
        line(format!("  title {}", q(file.title().as_deref())));
        line(format!("  performers {}", quoted_list(&file.performers())));
        line(format!("  albumartists {}", quoted_list(&file.album_artists())));
        line(format!("  composers {}", quoted_list(&file.composers())));
        line(format!("  album {}", q(file.album().as_deref())));
        line(format!("  genres {}", quoted_list(&file.genres())));
        line(format!(
            "  year {year} track {}/{} disc {}/{} bpm {bpm}",
            file.track(),
            file.track_count(),
            file.disc(),
            file.disc_count()
        ));
        line(format!("  copyright {}", q(file.copyright().as_deref())));
        line(format!("  lyrics {}", q(file.lyrics().as_deref())));
        line(format!(
            "  isrc {} publisher {}",
            q(file.isrc().as_deref()),
            q(file.publisher().as_deref())
        ));
        line(format!(
            "  releaseid {} releasegroupid {}",
            q(file.music_brainz_release_id().as_deref()),
            q(file.music_brainz_release_group_id().as_deref())
        ));
        for picture in file.pictures() {
            line(format!(
                "  picture {} {} {} {}",
                picture_type(picture.picture_type),
                q(Some(&picture.mime_type)),
                q(Some(&picture.description)),
                data(&picture.data)
            ));
        }

        line("extras".into());
        for (name, field) in FIELDS {
            line(format!(
                "  {name} {}",
                q(extras::read_text(&file, field).as_deref())
            ));
        }
        let recording = extras::read_recording_id(&mut file);
        line(format!(
            "  recordingid {} compilation {}",
            q(recording.as_deref()),
            boolean(extras::is_compilation(&file))
        ));
    }

    let text = path.to_str().expect("a UTF-8 path");
    let facts = extras::read_facts(text, true);
    let isrcs: Vec<String> = facts.isrcs.iter().map(|code| q(Some(code))).collect();
    line(format!(
        "facts {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        facts.duration_seconds,
        q(Some(&facts.extension)),
        facts.sample_rate,
        q(facts.title.as_deref()),
        q(facts.artist.as_deref()),
        q(facts.album.as_deref()),
        q(facts.album_artist.as_deref()),
        number(facts.year),
        number(facts.track),
        number(facts.disc),
        list(&isrcs),
        q(facts.barcode.as_deref()),
        q(facts.catalog_number.as_deref()),
        q(facts.label.as_deref()),
        q(facts.recording_id.as_deref()),
        q(facts.release_id.as_deref()),
        boolean(facts.is_compilation),
        boolean(facts.tags_are_evidence)
    ));
    let bare = extras::read_facts(text, false);
    line(format!(
        "barefacts {} {} {} {}",
        bare.duration_seconds,
        q(Some(&bare.extension)),
        bare.sample_rate,
        boolean(bare.tags_are_evidence)
    ));
    let (recording, seconds) = extras::read_identity(path);
    line(format!("identity {} {seconds}", q(recording.as_deref())));
    let (album, album_artist, compilation) = extras::read_album(path);
    line(format!(
        "album {} {} {}",
        q(album.as_deref()),
        q(album_artist.as_deref()),
        boolean(compilation)
    ));
    match KeptIdentityTags::read(path, None) {
        None => line("kept null".into()),
        Some(kept) => {
            line(format!(
                "kept {} {} {} {} {} {} {} {} {}/{} {}/{} {}",
                q(Some(&kept.title)),
                q(kept.album.as_deref()),
                quoted_list(&kept.album_artist),
                quoted_list(&kept.album_artists),
                q(kept.album_version.as_deref()),
                q(kept.release_date.as_deref()),
                q(kept.album_id.as_deref()),
                q(kept.release_track_id.as_deref()),
                kept.track,
                kept.track_count,
                kept.disc,
                kept.disc_count,
                boolean(kept.compilation)
            ));
            line(format!("pid {}", q(Some(&KeptIdentityTags::pid_inputs(&kept)))));
        }
    }
    o
}

fn frame_line(frame: &Frame<'static>) -> String {
    let encoding = |encoding: lofty::TextEncoding| encoding as u8;
    match frame {
        Frame::UserText(text) => format!(
            "TXXX enc {} desc {} {}",
            encoding(text.encoding),
            q(Some(&text.description)),
            quoted_list(&split(&text.content))
        ),
        Frame::Text(text) => {
            format!(
                "{} enc {} {}",
                frame.id_str(),
                encoding(text.encoding),
                quoted_list(&split(&text.value))
            )
        }
        Frame::UniqueFileIdentifier(ufid) => format!(
            "UFID owner {} id {}",
            q(Some(&ufid.owner)),
            q(Some(&String::from_utf8_lossy(&ufid.identifier)))
        ),
        Frame::UnsynchronizedText(uslt) => format!(
            "USLT enc {} lang {} desc {} text {}",
            encoding(uslt.encoding),
            q(Some(&String::from_utf8_lossy(&uslt.language))),
            q(Some(&uslt.description)),
            q(Some(&uslt.content))
        ),
        Frame::Comment(comm) => format!(
            "COMM enc {} lang {} desc {} text {}",
            encoding(comm.encoding),
            q(Some(&String::from_utf8_lossy(&comm.language))),
            q(Some(&comm.description)),
            q(Some(&comm.content))
        ),
        Frame::Picture(apic) => format!(
            "APIC enc {} mime {} type {} desc {} {}",
            encoding(apic.encoding),
            q(Some(
                apic.picture.mime_type().map(|m| m.as_str()).unwrap_or_default()
            )),
            picture_type(apic.picture.pic_type().as_u8()),
            q(Some(apic.picture.description().unwrap_or_default())),
            data(apic.picture.data())
        ),
        other => format!("{} other", other.id_str()),
    }
}

/// A joined frame's values as TagLib# lists them (the model's own split).
fn split(joined: &str) -> Vec<String> {
    let mut values: Vec<String> = joined
        .split('\0')
        .map(|value| value.strip_prefix('\u{FEFF}').unwrap_or(value).to_string())
        .collect();
    while values.last().is_some_and(String::is_empty) {
        values.pop();
    }
    values
}

fn atom_line(apple_box: &AppleBox) -> String {
    let data_part = |value: &DataBox| {
        if value.flags == 1 {
            format!("data({}) {}", value.flags, q(value.as_text().as_deref()))
        } else {
            format!("data({}) {}", value.flags, data(&value.data))
        }
    };
    match apple_box {
        AppleBox::Item { name, data } => {
            let name: String = name.iter().map(|byte| char::from(*byte)).collect();
            let parts: Vec<String> = data.iter().map(data_part).collect();
            format!("{} {}", q(Some(&name)), list(&parts))
        }
        AppleBox::Dash { mean, name, data } => {
            let mut parts = vec![
                format!("mean {}", q(Some(mean))),
                format!("name {}", q(Some(name))),
            ];
            parts.extend(data.iter().map(data_part));
            format!("{} {}", q(Some("----")), list(&parts))
        }
    }
}

fn block_picture(base64: &str) -> String {
    match Picture::from_flac_bytes(base64.as_bytes(), true, lofty::config::ParsingMode::Relaxed) {
        Ok((picture, info)) => flac_picture(&picture, &info),
        Err(_) => format!("unreadable {}", q(Some(base64))),
    }
}

fn flac_picture(picture: &Picture, info: &PictureInformation) -> String {
    format!(
        "{} {} {} {}x{}x{}/{} {}",
        picture_type(picture.pic_type().as_u8()),
        q(Some(picture.mime_type().map(|m| m.as_str()).unwrap_or_default())),
        q(Some(picture.description().unwrap_or_default())),
        info.width,
        info.height,
        info.color_depth,
        info.num_colors,
        data(picture.data())
    )
}

/// TagLib#'s `PictureType` names.
fn picture_type(value: u8) -> String {
    const NAMES: [&str; 21] = [
        "Other",
        "FileIcon",
        "OtherFileIcon",
        "FrontCover",
        "BackCover",
        "LeafletPage",
        "Media",
        "LeadArtist",
        "Artist",
        "Conductor",
        "Band",
        "Composer",
        "Lyricist",
        "RecordingLocation",
        "DuringRecording",
        "DuringPerformance",
        "MovieScreenCapture",
        "ColoredFish",
        "Illustration",
        "BandLogo",
        "PublisherLogo",
    ];
    match value {
        255 => "NotAPicture".into(),
        value => NAMES
            .get(value as usize)
            .map_or_else(|| value.to_string(), |name| name.to_string()),
    }
}

fn data(bytes: &[u8]) -> String {
    let hex = |bytes: &[u8]| {
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    };
    if bytes.len() <= 16 {
        format!("hex {}", hex(bytes))
    } else {
        format!(
            "bytes {} sha256 {}",
            bytes.len(),
            &hex(&Sha256::digest(bytes))[..16]
        )
    }
}

fn number(value: Option<i32>) -> String {
    value.map_or_else(|| "null".into(), |value| value.to_string())
}

fn boolean(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn list(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

fn quoted_list(items: &[String]) -> String {
    list(&items.iter().map(|item| q(Some(item))).collect::<Vec<_>>())
}

/// `Dumper.Q`: quoted, with only '\\', '"' and control characters escaped.
pub(crate) fn q(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "null".into();
    };
    let mut out = String::from("\"");
    for c in value.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
            out.push(c);
        } else if (c as u32) < 0x20 {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}
