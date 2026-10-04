//! The data half of `Services/Metadata/DeezerMetadataService.cs`: the records it answers with
//! and the record-type words. The service itself (HTTP, caches, matching) is
//! `octo::services::metadata::deezer_metadata_service`.

use crate::common::dotnet;

/// What a track search tells about one song.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackMeta {
    pub album_title: Option<String>,
    pub album_cover_url: Option<String>,
    pub year: Option<i32>,
    pub duration: Option<i32>,
    pub artist_name: Option<String>,
    pub artist_image_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArtistMeta {
    pub name: Option<String>,
    pub image_url: Option<String>,
}

/// Everything Deezer knows about a track, for writing rich file tags. Contributors is every
/// Main and Featured artist, in order; the search hit only names the main one, so it comes
/// from the track's own record. AlbumArtistName and RecordType come from the album, and are
/// what tell a compilation apart: Deezer usually reports one as record_type "album", so the
/// album artist "Various Artists" is the signal that holds.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FullTrackMeta {
    pub album_title: Option<String>,
    pub album_cover_url: Option<String>,
    pub year: Option<i32>,
    pub duration: Option<i32>,
    pub artist_name: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub isrc: Option<String>,
    pub total_tracks: Option<i32>,
    pub genre: Option<String>,
    pub label: Option<String>,
    pub release_date: Option<String>,
    pub contributors: Option<Vec<String>>,
    pub album_artist_name: Option<String>,
    pub record_type: Option<String>,

    /// The track's own title and ids in the catalog, the album's barcode, and what
    /// the catalog says about the words and the loudness, for the chooser and its report.
    pub title: Option<String>,
    pub track_id: Option<String>,
    pub album_id: Option<String>,
    pub barcode: Option<String>,
    pub explicit_lyrics: Option<bool>,
    pub catalog_gain: Option<f64>,
}

/// The catalog's ranked answers for one song, or the fact that it did not answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CatalogCandidates {
    pub hits: Vec<FullTrackMeta>,
    pub did_not_answer: bool,
}

/// One album from a catalog search. Year is not on the search payload;
/// the detail call fills it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AlbumHit {
    pub deezer_id: String,
    pub title: String,
    pub artist: String,
    pub cover_url: Option<String>,
    pub year: Option<i32>,
    pub track_count: i32,
    pub record_type: Option<String>,
}

/// One track of an album, with the real length and position.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AlbumTrack {
    pub title: String,
    pub artist: String,
    pub duration: Option<i32>,
    pub track_position: Option<i32>,
    pub disc_number: Option<i32>,
    pub isrc: Option<String>,
}

/// An album plus its full tracklist. RecordType is the catalog's own word for it:
/// album, ep, single or compile.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AlbumDetail {
    pub deezer_id: String,
    pub title: String,
    pub artist: String,
    pub cover_url: Option<String>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub label: Option<String>,
    pub tracks: Vec<AlbumTrack>,
    pub record_type: Option<String>,
}

/// What the catalog said when asked for an album's detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlbumAnswer {
    /// The album and its tracklist.
    Found,
    /// Deezer answered, and it has no such album.
    NoSuchAlbum,
    /// Deezer knows the album, but its tracklist came back empty.
    NoTracks,
    /// Deezer did not answer this time: throttled, unreachable or unreadable.
    Unavailable,
}

/// An album's detail, when there is one, and what the catalog said.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumLookup {
    pub detail: Option<AlbumDetail>,
    pub answer: AlbumAnswer,
}

/// One artist from a catalog search. Fans is how many follow them, which is what
/// tells two artists of one name apart when nothing better is known.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArtistHit {
    pub deezer_id: String,
    pub name: String,
    pub picture_url: Option<String>,
    pub album_count: i32,
    pub fans: i32,
}

/// A catalog record type as release types, in the lowercase words Navidrome relays from the
/// tags (MusicBrainz's): "album", "ep", "single". An outside album sits beside library
/// albums on one artist's page, and the two used to disagree on case. The catalog says
/// "compile" for a compilation, which MusicBrainz files as an album that is a compilation.
/// None for a type with no such word.
pub fn release_types(record_type: Option<&str>) -> &'static [&'static str] {
    match normalized_record_type(record_type).as_deref() {
        Some("album") => &["album"],
        Some("ep") => &["ep"],
        Some("single") => &["single"],
        Some("compile") => &["album", "compilation"],
        _ => &[],
    }
}

/// The catalog's record type in one spelling: lowercase, "compile" for either
/// word for a compilation.
pub fn normalized_record_type(record_type: Option<&str>) -> Option<String> {
    let kind = dotnet::to_lower_invariant(record_type?.trim());
    Some(if kind == "compilation" {
        "compile".to_string()
    } else {
        kind
    })
}

/// Where a record type sits on an artist's page: albums, EPs, singles, compilations,
/// then anything the catalog did not name.
pub fn release_rank(record_type: Option<&str>) -> i32 {
    match normalized_record_type(record_type).as_deref() {
        Some("album") => 0,
        Some("ep") => 1,
        Some("single") => 2,
        Some("compile") => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of `ReleaseTypes_AreNavidromesLowercaseWords` (8 rows).
    #[test]
    fn release_types_are_navidromes_lowercase_words() {
        let cases: &[(Option<&str>, &str)] = &[
            (Some("album"), "album"),
            (Some("ep"), "ep"),
            (Some("single"), "single"),
            (Some("compile"), "album,compilation"),
            (Some("compilation"), "album,compilation"),
            (Some("ALBUM"), "album"),
            (Some("mixtape"), ""),
            (None, ""),
        ];
        for &(record_type, expected) in cases {
            assert_eq!(release_types(record_type).join(","), expected, "{record_type:?}");
        }
    }
}
