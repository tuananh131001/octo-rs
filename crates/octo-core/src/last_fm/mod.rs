//! Last.fm, the pure parts (`Services/LastFm`): the records the web service's answers become,
//! the cleanup of `track.search` rows, the scrobble request signature, and the radio's rules
//! (seeds, spacing, refresh policy, the weighted draw, the flow picker and the cache profile).
//! The clients and the radio services themselves are `octo::services::last_fm`.

pub mod last_fm_radio_audio_transcoder;
pub mod last_fm_radio_recommendation_service;
pub mod last_fm_radio_refresh_policy;
pub mod last_fm_radio_seed_normalizer;
pub mod last_fm_radio_spacing;
pub mod last_fm_radio_state_store;
pub mod last_fm_radio_stream_service;
pub mod last_fm_radio_track_resolver;
pub mod last_fm_scrobble_service;
pub mod last_fm_search_cleanup;
pub mod last_fm_service;

pub use last_fm_radio_audio_transcoder::RadioAudioProfile;
pub use last_fm_scrobble_service::{
    LastFmCredentialCheck, LastFmScrobbleException, LastFmScrobbleUser, LastFmSentPlay, LastFmTrack,
};
pub use last_fm_search_cleanup::LastFmSearchCleanup;
pub use last_fm_service::{SimilarArtist, SimilarTrack, TrackInfo};
