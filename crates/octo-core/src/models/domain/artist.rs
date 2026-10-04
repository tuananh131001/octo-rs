//! Port of `Models/Domain/Artist.cs`.

use serde::{Deserialize, Serialize};

/// Represents an artist
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub album_count: Option<i32>,
    pub is_local: bool,
    pub external_provider: Option<String>,
    pub external_id: Option<String>,
}
