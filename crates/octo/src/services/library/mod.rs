//! `Services/Library` in the C#: library actions, the upgrade queue, the generated mixes and the
//! Navidrome song and playlist plumbing.

pub mod duplicate_scan_worker;
pub mod generated_playlist_service;
pub mod heart_ownership;
pub mod library_action_executor;
pub mod library_action_journal;
pub mod library_action_playlist_provisioner;
pub mod library_action_playlist_worker;
pub mod library_action_quarantine;
pub mod library_action_rating_worker;
pub mod library_ownership;
pub mod library_review_sweep_worker;
pub mod navidrome_playlist_api;
pub mod navidrome_song_list;
pub mod navidrome_song_path_resolver;
pub mod notice_playlist_worker;
pub mod notice_queue;
pub mod quality_upgrade_worker;
pub mod replacement_handoff;
mod state_dictionary;
pub mod upgrade_queue;
pub mod upgrade_sources;

#[cfg(test)]
mod library_action_keep_file_tests;
#[cfg(test)]
pub(crate) mod library_action_test_support;

pub use duplicate_scan_worker::{DuplicateGroup, DuplicateScanResult, DuplicateScanWorker, LibraryTrack};
pub use generated_playlist_service::{GeneratedPlaylist, GeneratedPlaylistService};
pub use heart_ownership::HeartOwnership;
pub use library_action_executor::{
    LibraryActionCodes, LibraryActionExecutor, LibraryActionExecutorParts, LibraryActionOutcome,
    LibraryActionRequest,
};
pub use library_action_journal::{LibraryActionEntry, LibraryActionJournal, LibraryActionState};
pub use library_action_playlist_provisioner::LibraryActionPlaylistProvisioner;
pub use library_action_playlist_worker::LibraryActionPlaylistWorker;
pub use library_action_quarantine::{LibraryActionQuarantine, QuarantineManifest, QuarantineResult};
pub use library_action_rating_worker::{LibraryActionRatingWorker, RatingActionRequest};
pub use library_ownership::{LibraryOwnership, OwnedCopy, OwnedDecision, OwnershipNavidrome};
pub use library_review_sweep_worker::{
    FingerprintSweepVerifier, IReviewSweepVerifier, LibraryReviewSweepParts, LibraryReviewSweepWorker,
    ReviewSweepState, ReviewSweepStatus, ReviewSweepStore, SweepVerification,
};
pub use navidrome_playlist_api::NavidromePlaylistApi;
pub use navidrome_song_list::NavidromeSongEntry;
pub use navidrome_song_path_resolver::{NavidromeSongPathResolver, PathSource, ResolvedSongFile};
pub use notice_playlist_worker::{NoticePlan, NoticePlaylistWorker, NoticeReconcile};
pub use notice_queue::{NoticeEntry, NoticeOrigin, NoticeQueue, NoticeState};
pub use quality_upgrade_worker::{
    LibrarySongRow, QualityUpgradeAttempt, QualityUpgradeState, QualityUpgradeStatus, QualityUpgradeStore,
    QualityUpgradeWorker,
};
pub use replacement_handoff::{ReplacementHandoff, ReplacementRejectedException};
pub use upgrade_queue::{
    AudioSummary, UpgradeAsk, UpgradeJob, UpgradeQueue, UpgradeResult, UpgradeStates, UpgradeWorker,
    UpgradeWorkerParts, UpgradeWorkerSeams,
};
pub use upgrade_sources::UpgradeSources;
