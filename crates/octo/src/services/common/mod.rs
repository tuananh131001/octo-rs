//! `Services/Common`: the shared services with state or I/O.

pub mod acquisition_activity;
pub mod acquisition_tracker;
pub mod acquisition_worker;
pub mod cache_cleanup_service;
pub mod download_concurrency;
pub mod external_search_service;
pub mod heart_acquisition_coordinator;
pub mod soulseek_hold_resumer;
pub mod soulseek_hold_store;
pub mod star_on_arrival;
pub mod track_acquisition_queue;

pub use acquisition_activity::{AcquisitionActivity, IAcquisitionActivity};
pub use acquisition_tracker::{
    AcquisitionEnd, AcquisitionSnapshot, AcquisitionState, AcquisitionTracker, TrackerServices, WatchSettings,
};
pub use acquisition_worker::AcquisitionWorker;
pub use cache_cleanup_service::CacheCleanupService;
pub use download_concurrency::{DownloadConcurrency, TransferLimiter, TransferSlot};
pub use external_search_service::ExternalSearchService;
pub use heart_acquisition_coordinator::{HeartAcquisitionCoordinator, HeartCoordinatorExtras};
pub use soulseek_hold_resumer::SoulseekHoldResumer;
pub use soulseek_hold_store::{HeldAcquisition, HeldKind, SoulseekHoldStore};
pub use star_on_arrival::{StarNavidrome, StarOnArrival};
pub use track_acquisition_queue::{
    AcquisitionOutcome, AcquisitionRequest, Completion, TrackAcquisitionQueue,
};

#[cfg(test)]
pub(crate) mod test_fakes;
