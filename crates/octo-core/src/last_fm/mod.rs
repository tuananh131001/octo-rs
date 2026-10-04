//! Last.fm, the pure parts (`Services/LastFm`): the records the web service's answers become,
//! the cleanup of `track.search` rows, and the scrobble request signature. The clients
//! themselves are `octo::services::last_fm`.

pub mod last_fm_scrobble_service;
pub mod last_fm_search_cleanup;
pub mod last_fm_service;

pub use last_fm_scrobble_service::{
    LastFmCredentialCheck, LastFmScrobbleException, LastFmScrobbleUser, LastFmSentPlay, LastFmTrack,
};
pub use last_fm_search_cleanup::LastFmSearchCleanup;
pub use last_fm_service::{SimilarArtist, SimilarTrack, TrackInfo};
