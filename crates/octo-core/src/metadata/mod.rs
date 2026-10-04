//! Metadata, the pure parts (`Services/Metadata`): the genre normaliser and the Accept-Language
//! header. The genre backfill's stores are `octo::services::metadata`; the Deezer client comes
//! with the metadata clients.

pub mod accept_language_header;
pub mod genre_normalizer;

pub use accept_language_header::{AcceptLanguageHeader, StringWithQuality};
pub use genre_normalizer::{GenreNormalizationResult, GenreNormalizer, GenreTagAction, GenreTagPlan};
