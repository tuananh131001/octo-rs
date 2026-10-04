//! `Services/Lidarr`: the Lidarr client, hearts through Lidarr, and one song through Lidarr for
//! a replacement. The records and the pure matching are `octo_core::lidarr`.

pub mod lidarr_album_claims;
pub mod lidarr_client;
pub mod lidarr_heart_acquisition_service;
pub mod lidarr_import_handoff;
pub mod lidarr_track_fetcher;

#[cfg(test)]
pub(crate) mod fake_lidarr;

pub use lidarr_album_claims::LidarrAlbumClaims;
pub use lidarr_client::LidarrClient;
pub use lidarr_heart_acquisition_service::{
    ILidarrHeartAcquisitionService, LidarrHeartAcquisitionService, LidarrHeartExtras,
};
pub use lidarr_import_handoff::{LidarrImport, LidarrImportHandoff};
pub use lidarr_track_fetcher::{ILidarrTrackFetcher, LidarrTrackFetcher};
pub use octo_core::lidarr::{
    LidarrAlbumCandidate, LidarrAlbumImportState, LidarrAlbumState, LidarrChoice, LidarrError,
    LidarrImportedTrack, LidarrOptions, LidarrRootFolder, LidarrSearchStarted, LidarrTrackRequest,
};
