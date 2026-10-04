//! STUB(3-D): replaced when 3-D lands with the port of `Services/Library/KeptIdentity.cs`. Only
//! the record is here, with 3-D's fields, so `ReplacementHandoff` (4-D) can carry one.

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
