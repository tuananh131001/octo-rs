//! `Services/Library` in the C#.

pub mod duplicate_scan_worker;
pub mod heart_ownership;
pub mod library_action_playlist_worker;
pub mod library_ownership;
pub mod navidrome_playlist_api;
pub mod navidrome_song_path_resolver;
pub mod quality_upgrade_worker;
pub mod replacement_handoff;
pub mod upgrade_queue;
pub mod upgrade_sources;

pub use heart_ownership::HeartOwnership;
pub use library_ownership::{LibraryOwnership, OwnedCopy, OwnedDecision, OwnershipNavidrome};
pub use navidrome_playlist_api::NavidromePlaylistApi;
pub use navidrome_song_path_resolver::{NavidromeSongPathResolver, PathSource, ResolvedSongFile};
pub use replacement_handoff::{ReplacementHandoff, ReplacementRejectedException};
pub use upgrade_queue::{UpgradeAsk, UpgradeJob, UpgradeQueue};
pub use upgrade_sources::UpgradeSources;
