//! Port of `Services/Common/AcquisitionTracker.cs`: only the snapshot a reader sees, as far as
//! getAcquisitions writes it.
//!
//! STUB(4-D): replaced when 4-D (acquisition orchestration) lands with the tracker itself. The
//! types keep their C# names, members and order so the swap changes no caller.

use chrono::{DateTime, Utc};

/// Where an acquisition is. Written lowercase on the wire (`State.ToString().ToLowerInvariant()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AcquisitionState {
    /// Accepted, waiting for the worker.
    #[default]
    Queued,
    /// Looking for a source or a peer.
    Searching,
    /// Bytes are flowing.
    Downloading,
    /// The duration check and the fingerprint.
    Verifying,
    /// Placed, tagged and registered; waiting for Navidrome to show it.
    Importing,
    Done,
    Failed,
}

impl AcquisitionState {
    /// The C# member name.
    pub fn name(self) -> &'static str {
        match self {
            AcquisitionState::Queued => "Queued",
            AcquisitionState::Searching => "Searching",
            AcquisitionState::Downloading => "Downloading",
            AcquisitionState::Verifying => "Verifying",
            AcquisitionState::Importing => "Importing",
            AcquisitionState::Done => "Done",
            AcquisitionState::Failed => "Failed",
        }
    }
}

/// One acquisition as a reader sees it. A copy, so it never changes under them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AcquisitionSnapshot {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub requested_by: Vec<String>,
    pub source: Option<String>,
    pub state: AcquisitionState,
    pub progress: Option<f64>,
    pub bytes_done: Option<i64>,
    pub bytes_total: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error: Option<String>,
    pub library_id: Option<String>,
    pub ahead: Option<i32>,
    pub note: Option<String>,
}
