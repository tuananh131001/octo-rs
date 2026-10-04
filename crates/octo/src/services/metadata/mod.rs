//! Metadata (`Services/Metadata`): the Deezer catalog client and the named client and rate
//! limiter every Deezer call goes through.

pub mod deezer_metadata_service;
pub mod deezer_rate_limit_handler;
pub mod deezer_rate_limiter;

pub use deezer_metadata_service::DeezerMetadataService;
pub use deezer_rate_limit_handler::DeezerRateLimitHandler;
pub use deezer_rate_limiter::DeezerRateLimiter;
