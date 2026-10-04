//! Metadata with I/O (`Services/Metadata`): the genre backfill's run state and undo journal.
//! The genre normaliser and the Accept-Language header are `octo_core::metadata`.

pub mod genre_backfill_journal;
pub mod genre_backfill_state;

pub use genre_backfill_journal::{GenreBackfillJournal, GenreJournalEntry};
pub use genre_backfill_state::{
    GenreBackfillChange, GenreBackfillRun, GenreBackfillScope, GenreBackfillStatus, GenreBackfillStore,
};
