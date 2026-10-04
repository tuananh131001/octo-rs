//! Metadata with I/O (`Services/Metadata`): the Deezer catalog client with the named client and
//! rate limiter every Deezer call goes through, and the genre backfill's run state, undo
//! journal and worker. The genre normaliser and the Accept-Language header are `octo_core::metadata`.

pub mod deezer_metadata_service;
pub mod deezer_rate_limit_handler;
pub mod deezer_rate_limiter;
pub mod genre_backfill_journal;
pub mod genre_backfill_state;
pub mod genre_backfill_worker;

pub use deezer_metadata_service::DeezerMetadataService;
pub use deezer_rate_limit_handler::DeezerRateLimitHandler;
pub use deezer_rate_limiter::DeezerRateLimiter;
pub use genre_backfill_journal::{GenreBackfillJournal, GenreJournalEntry};
pub use genre_backfill_state::{
    GenreBackfillChange, GenreBackfillRun, GenreBackfillScope, GenreBackfillStatus, GenreBackfillStore,
};
pub use genre_backfill_worker::{GenreBackfillRequest, GenreBackfillWorker};
