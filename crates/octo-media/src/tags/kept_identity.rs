//! Port of `Services/Library/KeptIdentity.cs`: reads a file's identity the way Navidrome 0.64
//! does and writes it onto a replacement (W8). Navidrome keeps a moved song's id, and every
//! play, favorite and playlist place on it, only when the new file has the same persistent id
//! as the one that went missing. Key lists are resources/mappings.yaml's aliases, lowercased,
//! in its order.

use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use octo_core::common::dotnet::{eq_ignore_case, is_null_or_white_space, to_lower_invariant};

use super::apple::names;
use super::tag_file::{Format, TagError, TagFile};
use super::tag_writer_extras::{self as extras, APPLE_MEAN, TagFields};

/// The tag values Navidrome builds a song's track and album ids from, as it reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptIdentity {
    pub title: String,
    pub album: Option<String>,
    pub album_artist: Vec<String>,
    pub album_artists: Vec<String>,
    pub album_version: Option<String>,
    pub release_date: Option<String>,
    pub album_id: Option<String>,
    pub release_track_id: Option<String>,
    pub track: u32,
    pub track_count: u32,
    pub disc: u32,
    pub disc_count: u32,
    pub compilation: bool,
}

/// `KeptIdentityTags`: reading an identity, writing it onto a replacement, and Navidrome's
/// own readings of dates and ids.
pub struct KeptIdentityTags;

pub const TITLE_KEYS: [&str; 4] = ["tit2", "title", "©nam", "inam"];
pub const ALBUM_KEYS: [&str; 5] = ["talb", "album", "©alb", "wm/albumtitle", "iprd"];
pub const ALBUM_ARTIST_KEYS: [&str; 6] = [
    "tpe2",
    "albumartist",
    "album artist",
    "album_artist",
    "aart",
    "wm/albumartist",
];
pub const ALBUM_ARTISTS_KEYS: [&str; 2] = ["txxx:album artists", "albumartists"];
pub const ALBUM_VERSION_KEYS: [&str; 3] = [
    "albumversion",
    "musicbrainz_albumcomment",
    "musicbrainz album comment",
];
pub const RELEASE_DATE_KEYS: [&str; 5] = ["tdrl", "releasedate", "©day", "wm/year", "year"];
pub const ALBUM_ID_KEYS: [&str; 5] = [
    "txxx:musicbrainz album id",
    "musicbrainz_albumid",
    "musicbrainz album id",
    "----:com.apple.itunes:musicbrainz album id",
    "musicbrainz/album id",
];
pub const RELEASE_TRACK_ID_KEYS: [&str; 4] = [
    "txxx:musicbrainz release track id",
    "musicbrainz_releasetrackid",
    "----:com.apple.itunes:musicbrainz release track id",
    "musicbrainz/release track id",
];
pub const ARTIST_KEYS: [&str; 5] = ["tpe1", "artist", "©art", "author", "iart"];
pub const ARTISTS_KEYS: [&str; 4] = [
    "txxx:artists",
    "artists",
    "----:com.apple.itunes:artists",
    "wm/artists",
];

pub const VARIOUS_ARTISTS: &str = "Various Artists";
pub const UNKNOWN_ARTIST: &str = "[Unknown Artist]";

/// Every custom field this owns, removed from a replacement before the copy.
fn is_owned(key: &str) -> bool {
    ALBUM_ARTIST_KEYS.contains(&key)
        || ALBUM_ARTISTS_KEYS.contains(&key)
        || ALBUM_VERSION_KEYS.contains(&key)
        || (RELEASE_DATE_KEYS.contains(&key) && key != "©day")
        || ALBUM_ID_KEYS.contains(&key)
        || RELEASE_TRACK_ID_KEYS.contains(&key)
}

/// The key a name is filed under: the two names TagLib itself renames on read (its TXXX and
/// MP4 freeform tables), and every other name lowercased.
fn key(name: &str) -> String {
    if eq_ignore_case(name, "MusicBrainz Album Id") {
        "musicbrainz_albumid".into()
    } else if eq_ignore_case(name, "MusicBrainz Release Track Id") {
        "musicbrainz_releasetrackid".into()
    } else {
        to_lower_invariant(name)
    }
}

static ARTIST_SPLIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?: / | feat\. | feat | ft\. | ft |; )").expect("a valid pattern"));
static YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("([12][0-9][0-9][0-9])").expect("a valid pattern"));

/// The keys and values Navidrome's tag reader sees, for the fields that matter here.
pub type NavidromeView = IndexMap<String, Vec<String>>;

impl KeptIdentityTags {
    /// The original's identity, or None when the file cannot be read. Read while it is still in
    /// place. `navidrome_album_artist` is the album artist Navidrome stored for it, used only
    /// when the file names none.
    pub fn read(path: &Path, navidrome_album_artist: Option<&str>) -> Option<KeptIdentity> {
        let file = TagFile::open(path).ok()?;
        let view = Self::navidrome_view(&file);
        let mut album_artist = values(&view, &ALBUM_ARTIST_KEYS);
        let album_artists = values(&view, &ALBUM_ARTISTS_KEYS);
        // No album artist tag: Navidrome made one up, and that name is what its album id
        // hashes. Written as the tag it hashes the same, and the track artist stays new.
        if album_artist.is_empty() && album_artists.is_empty() {
            album_artist = vec![Self::fallback_album_artist(
                &view,
                navidrome_album_artist,
                extras::is_compilation(&file),
            )];
        }
        Some(KeptIdentity {
            // No title tag: Navidrome titles it by its file name, which the replacement keeps.
            title: values(&view, &TITLE_KEYS)
                .into_iter()
                .next()
                .unwrap_or_else(|| file_name_without_extension(path)),
            album: values(&view, &ALBUM_KEYS).into_iter().next(),
            album_artist,
            album_artists,
            album_version: values(&view, &ALBUM_VERSION_KEYS).into_iter().next(),
            release_date: values(&view, &RELEASE_DATE_KEYS)
                .iter()
                .find_map(|value| Self::navidrome_date(value)),
            album_id: values(&view, &ALBUM_ID_KEYS)
                .iter()
                .find_map(|value| Self::navidrome_uuid(value)),
            release_track_id: values(&view, &RELEASE_TRACK_ID_KEYS)
                .iter()
                .find_map(|value| Self::navidrome_uuid(value)),
            track: file.track(),
            track_count: file.track_count(),
            disc: file.disc(),
            disc_count: file.disc_count(),
            compilation: extras::is_compilation(&file),
        })
    }

    /// Overwrite the replacement's identity with the original's and remove what the original
    /// lacked. Cover, ReplayGain, lyrics, genre, ISRC, label and recording id stay.
    pub fn apply(path: &Path, identity: &KeptIdentity) -> Result<(), TagError> {
        let mut file = TagFile::open(path)?;
        remove(&mut file);
        if let Some(id3) = &mut file.id3v2 {
            id3.remove_frames("TDRL");
            // Version 3 holds one album artist per frame; several need version 4.
            if identity.album_artist.len() > 1 && id3.version < 4 {
                id3.version = 4;
            }
        }
        // The date atom is a release date alias on MP4, so it goes when the original had none.
        if identity.release_date.is_none()
            && let Some(apple) = &mut file.apple
        {
            apple.clear_data(names::DAY);
        }

        file.set_title(Some(&identity.title));
        file.set_album(identity.album.as_deref());
        file.set_album_artists(&identity.album_artist);
        file.set_track(identity.track);
        file.set_track_count(identity.track_count);
        file.set_disc(identity.disc);
        file.set_disc_count(identity.disc_count);
        extras::set_compilation(&mut file, identity.compilation);
        extras::set_exact(&mut file, TagFields::ALBUM_ARTISTS, &identity.album_artists);
        let version: Vec<String> = identity.album_version.iter().cloned().collect();
        extras::set_exact(&mut file, TagFields::ALBUM_VERSION, &version);
        extras::set_text(
            &mut file,
            TagFields::RELEASE_DATE,
            identity.release_date.as_deref(),
        );
        extras::set_text(&mut file, TagFields::ALBUM_ID, identity.album_id.as_deref());
        extras::set_release_track_id(&mut file, identity.release_track_id.as_deref());
        file.save()
    }

    /// Navidrome's two persistent ids' inputs as one string: equal strings, equal ids.
    pub fn pid_inputs(identity: &KeptIdentity) -> String {
        [
            identity.release_track_id.clone().unwrap_or_default(),
            identity.album_id.clone().unwrap_or_default(),
            identity.album_artist.join("\u{1}"),
            identity.album_artists.join("\u{1}"),
            identity.album.clone().unwrap_or_default(),
            identity.album_version.clone().unwrap_or_default(),
            identity.release_date.clone().unwrap_or_default(),
            identity.title.clone(),
        ]
        .join("|")
    }

    /// The keys and values Navidrome's tag reader sees, for the fields that matter here. Only
    /// the container Navidrome reads: ID3 on MP3, Vorbis on FLAC and Ogg, iTunes atoms on MP4.
    pub fn navidrome_view(file: &TagFile) -> NavidromeView {
        let mut view = NavidromeView::new();
        let mut add = |key: String, values: Vec<String>| {
            view.entry(key)
                .or_default()
                .extend(values.into_iter().filter(|value| !value.is_empty()));
        };
        match (file.format(), &file.xiph, &file.apple, &file.id3v2) {
            (Format::Flac | Format::Ogg, Some(xiph), _, _) => {
                for (name, values) in xiph.fields.iter() {
                    add(to_lower_invariant(name), values.clone());
                }
            }
            (Format::Mp4, _, Some(apple), _) => {
                add("title".into(), apple.text(names::NAM));
                add("album".into(), apple.text(names::ALB));
                add("albumartist".into(), apple.text(names::AART));
                add("artist".into(), apple.text(names::ART));
                add("©day".into(), apple.text(names::DAY));
                for (name, _) in apple.freeform(APPLE_MEAN) {
                    let values = apple
                        .dash_boxes(APPLE_MEAN, &name)
                        .unwrap_or_default()
                        .into_iter()
                        .map(Option::unwrap_or_default)
                        .collect();
                    add(key(&name), values);
                }
            }
            (Format::Flac | Format::Ogg | Format::Mp4, _, _, _) => {}
            (_, _, _, Some(id3)) => {
                // TagLib# splits a version 3 TPE1/TPE2 at "/"; the reader Navidrome uses does not.
                let joined = |id: &str| {
                    let parts = id3.all_text_values(id);
                    if parts.len() > 1 && id3.version < 4 {
                        vec![parts.join("/")]
                    } else {
                        parts
                    }
                };
                add("title".into(), id3.all_text_values("TIT2"));
                add("album".into(), id3.all_text_values("TALB"));
                add("albumartist".into(), joined("TPE2"));
                add("artist".into(), joined("TPE1"));
                add("releasedate".into(), id3.all_text_values("TDRL"));
                for (description, values) in id3.user_texts() {
                    if !description.is_empty() {
                        add(key(&description), values);
                    }
                }
            }
            _ => {}
        }
        view
    }

    /// The album artist Navidrome named an album that names none (map_participants.go).
    pub fn fallback_album_artist(view: &NavidromeView, navidrome: Option<&str>, compilation: bool) -> String {
        if let Some(navidrome) = navidrome.filter(|name| !is_null_or_white_space(Some(name))) {
            return navidrome.to_string();
        }
        if compilation {
            return VARIOUS_ARTISTS.into();
        }
        if let Some(first) = values(view, &ARTISTS_KEYS).into_iter().next() {
            return first;
        }
        let artist = values(view, &ARTIST_KEYS);
        if artist.len() == 1 {
            return ARTIST_SPLIT
                .split(&artist[0])
                .find(|part| !part.is_empty())
                .map_or_else(|| artist[0].clone(), str::to_string);
        }
        artist.into_iter().next().unwrap_or_else(|| UNKNOWN_ARTIST.into())
    }

    /// Navidrome's parseDate (metadata.go:174-203): None when there is no year.
    pub fn navidrome_date(value: &str) -> Option<String> {
        let length = value.encode_utf16().count();
        if length < 4 {
            return None;
        }
        let year = YEAR.captures(value)?.get(1)?.as_str().to_string();
        if length < 5 {
            return Some(year);
        }
        let head: String = value.chars().take(10).collect();
        if is_date(&head, true) || is_date(&head, false) {
            Some(head)
        } else {
            Some(year)
        }
    }

    /// An id Navidrome keeps, in its canonical form; None for one it would drop.
    pub fn navidrome_uuid(value: &str) -> Option<String> {
        let units: Vec<char> = value.chars().collect();
        let text: String = if units.len() == 45
            && value
                .get(..9)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("urn:uuid:"))
        {
            units[9..].iter().collect()
        } else if units.len() == 38 {
            units[1..37].iter().collect()
        } else {
            value.to_string()
        };
        parse_guid(&text, text.chars().count() == 32)
    }
}

/// `Values(view, keys)`: every value under the keys, in key order, each once.
fn values(view: &NavidromeView, keys: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in keys {
        for value in view.get(*key).into_iter().flatten() {
            if !out.contains(value) {
                out.push(value.clone());
            }
        }
    }
    out
}

/// `Remove`: the owned custom fields, from every container's tag the file has.
fn remove(file: &mut TagFile) {
    if let Some(id3) = &mut file.id3v2 {
        id3.retain_user_texts(|description| !is_owned(&key(description)));
    }
    if let Some(xiph) = &mut file.xiph {
        for name in xiph.fields.keys() {
            if is_owned(&to_lower_invariant(&name)) {
                xiph.remove_field(&name);
            }
        }
    }
    if let Some(apple) = &mut file.apple {
        let owned: Vec<String> = apple
            .freeform(APPLE_MEAN)
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| is_owned(&key(name)))
            .collect();
        for name in owned {
            apple.set_dash_box(APPLE_MEAN, &name, None);
        }
    }
}

/// `DateTime.TryParseExact(head, "yyyy-MM-dd")` (or `"yyyy-MM"`), invariant culture.
fn is_date(text: &str, with_day: bool) -> bool {
    let bytes = text.as_bytes();
    let expected = if with_day { 10 } else { 7 };
    if bytes.len() != expected || bytes[4] != b'-' || (with_day && bytes[7] != b'-') {
        return false;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = &text[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (Some(year), Some(month)) = (number(0..4), number(5..7)) else {
        return false;
    };
    if year == 0 || !(1..=12).contains(&month) {
        return false;
    }
    if !with_day {
        return true;
    }
    let Some(day) = number(8..10) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days).contains(&day)
}

/// `Guid.TryParseExact(text, "N" or "D")`, written back as "D" in lower case.
fn parse_guid(text: &str, compact: bool) -> Option<String> {
    let hex: String = if compact {
        (text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit())).then(|| text.to_string())?
    } else {
        let groups: Vec<&str> = text.split('-').collect();
        let sizes = [8, 4, 4, 4, 12];
        if text.len() != 36
            || groups.len() != 5
            || groups
                .iter()
                .zip(sizes)
                .any(|(group, size)| group.len() != size || !group.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return None;
        }
        groups.concat()
    };
    let hex = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// `Path.GetFileNameWithoutExtension`.
fn file_name_without_extension(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(at) => name[..at].to_string(),
        None => name,
    }
}
