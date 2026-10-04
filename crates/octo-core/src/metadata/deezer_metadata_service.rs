//! The data half of `Services/Metadata/DeezerMetadataService.cs` that the release chooser reads:
//! a catalog track's full metadata and the ranked answers for one song. The service itself (HTTP,
//! cache, rate limiter) belongs to the `octo` crate's port (task 2-D), which builds these records.

/// Everything Deezer knows about a track, for writing rich file tags. Contributors is every
/// Main and Featured artist, in order; the search hit only names the main one, so it comes
/// from the track's own record. AlbumArtistName and RecordType come from the album, and are
/// what tell a compilation apart: Deezer usually reports one as record_type "album", so the
/// album artist "Various Artists" is the signal that holds.
#[derive(Debug, Clone, Default, PartialEq)]
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogCandidates {
    pub hits: Vec<FullTrackMeta>,
    pub did_not_answer: bool,
}
