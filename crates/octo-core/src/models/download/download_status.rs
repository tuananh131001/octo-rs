//! Port of `Models/Download/DownloadStatus.cs`.

use serde_repr::{Deserialize_repr, Serialize_repr};

/// Download status of a song. Written as its number, as System.Text.Json writes an enum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum DownloadStatus {
    #[default]
    NotStarted = 0,
    InProgress = 1,
    Completed = 2,
    Failed = 3,
}
