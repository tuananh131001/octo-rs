//! The records of `Services/LastFm/LastFmService.cs` (`LastFmService.SimilarTrack`,
//! `SimilarArtist` and `TrackInfo`). The client is `octo::services::last_fm::last_fm_service`.

/// One song Last.fm suggested or found.
///
/// `listeners`: how many people Last.fm counts for the row, where the call says.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarTrack {
    pub artist: String,
    pub title: String,
    pub r#match: f64,
    pub duration: Option<i32>,
    pub listeners: Option<i64>,
}

impl SimilarTrack {
    /// `new SimilarTrack(artist, title, match)`, with no duration and no listener count.
    pub fn new(artist: impl Into<String>, title: impl Into<String>, r#match: f64) -> Self {
        Self {
            artist: artist.into(),
            title: title.into(),
            r#match,
            duration: None,
            listeners: None,
        }
    }

    /// `with { Duration = .. }`.
    pub fn with_duration(mut self, duration: Option<i32>) -> Self {
        self.duration = duration;
        self
    }

    /// `with { Listeners = .. }`.
    pub fn with_listeners(mut self, listeners: Option<i64>) -> Self {
        self.listeners = listeners;
        self
    }
}

/// An artist `artist.getsimilar` named, with how alike Last.fm thinks they are.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarArtist {
    pub name: String,
    pub r#match: f64,
}

/// `track.getInfo`, as far as the radio reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<i32>,
    pub tags: Vec<String>,
}
