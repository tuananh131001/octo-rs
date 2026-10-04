//! Shared helpers: `Services/Common` in the C#, its pure parts.

pub mod acquisition_tracker;
pub mod clock;
pub mod dotnet;
pub mod dotnet_random;
pub mod error;
pub mod live_version;
pub mod log_redaction;
pub mod octo_user_agent;
pub mod path_helper;
pub mod playlist_id_helper;
pub mod single_flight;
pub mod song_identity;
pub mod supersedable_build_coordinator;

pub use clock::Clock;
pub use error::{Error, ErrorType};
pub use path_helper::PathHelper;
pub use single_flight::SingleFlight;
pub use song_identity::{
    ArtistAgreement, SongArtists, SongIdentity, SongMatch, SongMatchOptions, SongQuery, SongRef, SongTitle,
    SongVerdict,
};
pub use supersedable_build_coordinator::SupersedableBuildCoordinator;
