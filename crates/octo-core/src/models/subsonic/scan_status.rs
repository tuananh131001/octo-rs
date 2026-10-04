//! Port of `Models/Subsonic/ScanStatus.cs`.

use serde::{Deserialize, Serialize};

/// Subsonic library scan status
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub count: Option<i32>,
}
