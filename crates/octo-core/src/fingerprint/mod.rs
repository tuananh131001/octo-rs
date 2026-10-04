//! Fingerprinting: comparers, the AcoustID answer records and their reading, the pure half of the
//! MusicBrainz client, and the verification types (`Services/Fingerprint`, its pure parts).

pub mod acoust_id_client;
pub mod music_brainz_client;
pub mod track_match_comparer;
pub mod verification;

pub use acoust_id_client::{
    AcoustIdCredit, AcoustIdLookup, AcoustIdRecording, AcoustIdRelease, AcoustIdResult,
};
pub use track_match_comparer::TrackMatchComparer;
pub use verification::{InconclusiveReason, VerificationResult, VerificationVerdict};
