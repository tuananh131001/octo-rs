//! Fingerprint lookups (`Services/Fingerprint`): the AcoustID client with its named client and
//! rate limiter, and the MusicBrainz client.

pub mod acoust_id_client;
pub mod acoust_id_rate_limit_handler;
pub mod acoust_id_rate_limiter;
pub mod music_brainz_client;

pub use acoust_id_client::AcoustIdClient;
pub use acoust_id_rate_limit_handler::AcoustIdRateLimitHandler;
pub use acoust_id_rate_limiter::AcoustIdRateLimiter;
pub use music_brainz_client::{MusicBrainzClient, MusicBrainzError};
