//! Port of `Models/Search/SearchResult.cs`.

use serde::{Deserialize, Serialize};

use crate::models::domain::{album::Album, artist::Artist, song::Song};

/// Search result combining local and external results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SearchResult {
    pub songs: Vec<Song>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
}
