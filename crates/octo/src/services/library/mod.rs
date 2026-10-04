//! `Services/Library`: library actions, the upgrade queue and the generated mixes.
//!
//! STUB(5-A, 5-B, 5-C): only the types the Subsonic answers write exist so far.

pub mod generated_playlist_service;
pub mod library_action_executor;
pub mod library_action_journal;
pub mod upgrade_queue;

pub use generated_playlist_service::GeneratedPlaylist;
pub use library_action_executor::LibraryActionOutcome;
pub use library_action_journal::LibraryActionState;
pub use upgrade_queue::UpgradeJob;
