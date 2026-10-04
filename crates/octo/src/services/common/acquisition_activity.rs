//! Port of `Services/Common/AcquisitionActivity.cs`.

use std::sync::{Arc, OnceLock, Weak};

use super::TrackAcquisitionQueue;
use crate::services::i_download_service::IDownloadService;

/// Whether anything is downloading. The background library jobs wait until nothing is,
/// so they never get in front of a person.
pub trait IAcquisitionActivity: Send + Sync {
    fn is_busy(&self) -> bool;
}

/// A queued star or play, or any transfer at all: album tracks bypass the queue.
///
/// The C# resolved both from the `IServiceProvider` late, "because hosted services are built
/// before the download service is first used". The queue depends on nothing and is handed over
/// at construction. The download service is set once it exists
/// ([`AcquisitionActivity::set_downloads`]), and held weakly: the download service is what
/// reports to the library jobs that read this, so a strong reference would make a cycle.
pub struct AcquisitionActivity {
    queue: Arc<TrackAcquisitionQueue>,
    downloads: OnceLock<Weak<dyn IDownloadService>>,
}

impl AcquisitionActivity {
    pub fn new(queue: Arc<TrackAcquisitionQueue>) -> Self {
        AcquisitionActivity {
            queue,
            downloads: OnceLock::new(),
        }
    }

    /// The download service, once built. A second call is ignored.
    pub fn set_downloads(&self, downloads: &Arc<dyn IDownloadService>) {
        let _ = self.downloads.set(Arc::downgrade(downloads));
    }
}

impl IAcquisitionActivity for AcquisitionActivity {
    fn is_busy(&self) -> bool {
        !self.queue.is_idle()
            || self
                .downloads
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|downloads| downloads.has_active_downloads())
    }
}
