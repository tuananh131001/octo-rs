//! `Octo.Models.Settings.LidarrSettings` (the `Lidarr` section).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum LidarrCompletionMode {
    /// The heart is considered handed off once Lidarr accepts AlbumSearch.
    #[default]
    Accepted,

    /// Completion/failure notifications follow the actual import or timeout.
    Imported,
}

/// Connection and add-album defaults for an existing Lidarr server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LidarrSettings {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub root_folder_path: Option<String>,
    pub quality_profile_id: i32,
    pub metadata_profile_id: i32,
    pub completion_mode: LidarrCompletionMode,
    pub import_timeout_seconds: i32,
}

impl Default for LidarrSettings {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key: None,
            root_folder_path: None,
            quality_profile_id: 0,
            metadata_profile_id: 0,
            completion_mode: LidarrCompletionMode::Accepted,
            import_timeout_seconds: 1800,
        }
    }
}
