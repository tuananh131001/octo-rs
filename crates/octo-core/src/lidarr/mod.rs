//! The pure half of `Services/Lidarr`: the records Lidarr's answers are read into, how an album
//! is chosen from a lookup, and how an imported track is matched to a song. The client and the
//! services that talk to Lidarr are `octo::services::lidarr`.

pub mod lidarr_client;
pub mod lidarr_heart_acquisition_service;
pub mod lidarr_track_fetcher;

pub use lidarr_client::{
    LidarrAlbumCandidate, LidarrAlbumImportState, LidarrAlbumState, LidarrChoice, LidarrError,
    LidarrImportedTrack, LidarrOptions, LidarrRootFolder, LidarrSearchStarted,
};
pub use lidarr_track_fetcher::LidarrTrackRequest;
