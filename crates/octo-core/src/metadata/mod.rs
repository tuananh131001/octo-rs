//! Metadata, the pure parts (`Services/Metadata`): the genre normaliser, the Accept-Language
//! header, and the Deezer record types the tagging plan holds. The genre backfill's stores and
//! the Deezer client are `octo::services::metadata`.

pub mod accept_language_header;
pub mod deezer_metadata_service;
pub mod genre_normalizer;

pub use accept_language_header::{AcceptLanguageHeader, StringWithQuality};
pub use genre_normalizer::{GenreNormalizationResult, GenreNormalizer, GenreTagAction, GenreTagPlan};
