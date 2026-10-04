//! `Services/Library` in the C#: library actions, the upgrade queue, the generated mixes and the
//! Navidrome song and playlist plumbing.
//!
//! STUB(5-A, 5-B, 5-C): of the actions, the queue and the mixes, only the types the Subsonic
//! answers write exist so far.

pub mod duplicate_scan_worker;
pub mod generated_playlist_service;
pub mod heart_ownership;
pub mod library_action_executor;
pub mod library_action_journal;
pub mod library_action_playlist_worker;
pub mod library_ownership;
pub mod navidrome_playlist_api;
pub mod navidrome_song_path_resolver;
pub mod quality_upgrade_worker;
pub mod replacement_handoff;
pub mod upgrade_queue;
pub mod upgrade_sources;

pub use generated_playlist_service::GeneratedPlaylist;
pub use heart_ownership::HeartOwnership;
pub use library_action_executor::LibraryActionOutcome;
pub use library_action_journal::{LibraryActionEntry, LibraryActionJournal, LibraryActionState};
pub use library_ownership::{LibraryOwnership, OwnedCopy, OwnedDecision, OwnershipNavidrome};
pub use navidrome_playlist_api::NavidromePlaylistApi;
pub use navidrome_song_path_resolver::{NavidromeSongPathResolver, PathSource, ResolvedSongFile};
pub use replacement_handoff::{ReplacementHandoff, ReplacementRejectedException};
pub use upgrade_queue::{UpgradeAsk, UpgradeJob, UpgradeQueue};
pub use upgrade_sources::UpgradeSources;
