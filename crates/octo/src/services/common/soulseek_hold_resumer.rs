//! Port of `Services/Common/SoulseekHoldResumer.cs`, a worker of the supervisor.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{HeartAcquisitionCoordinator, HeldKind, SoulseekHoldStore};

/// Picks up the hearts a restart interrupted while they waited for Soulseek.
pub struct SoulseekHoldResumer {
    store: Arc<SoulseekHoldStore>,
    hearts: Arc<HeartAcquisitionCoordinator>,
}

impl SoulseekHoldResumer {
    pub fn new(store: Arc<SoulseekHoldStore>, hearts: Arc<HeartAcquisitionCoordinator>) -> Self {
        SoulseekHoldResumer { store, hearts }
    }

    /// `ExecuteAsync`: starts every held chain again and finishes at once.
    pub async fn run(self: Arc<Self>, _stopping: CancellationToken) -> anyhow::Result<()> {
        let held = self.store.snapshot();
        if !held.is_empty() {
            info!(
                "Resuming {} downloads that were waiting for Soulseek before the restart",
                held.len()
            );
        }
        for entry in held {
            if entry.kind == HeldKind::Album {
                self.hearts.resume_album(entry);
            } else {
                self.hearts.resume_track(entry);
            }
        }
        Ok(())
    }
}
