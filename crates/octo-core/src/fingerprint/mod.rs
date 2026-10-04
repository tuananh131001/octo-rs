//! Fingerprinting: comparers and verification types (`Services/Fingerprint`), and the pure halves
//! of the AcoustID and MusicBrainz clients.
//!
//! STUB(wave 2 fingerprint): `verification` and `track_match_comparer` hold only what other
//! modules need yet; replaced when the fingerprint port (2-A) lands.

pub mod acoust_id_client;
pub mod music_brainz_client;
pub mod track_match_comparer;
pub mod verification;
