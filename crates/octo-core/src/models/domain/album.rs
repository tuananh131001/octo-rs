//! Port of `Models/Domain/Album.cs`.

use serde::{Deserialize, Serialize};

use super::song::Song;

/// Represents an album
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub year: Option<i32>,
    pub song_count: Option<i32>,
    pub cover_art_url: Option<String>,
    pub genre: Option<String>,

    /// OpenSubsonic's release types, such as "album", "ep" or "single": lowercase, as
    /// Navidrome relays them for library albums. What lets a client group an artist's page
    /// the way the catalog does, without guessing from a track count.
    pub release_types: Vec<String>,
    pub is_local: bool,
    pub external_provider: Option<String>,
    pub external_id: Option<String>,
    pub songs: Vec<Song>,
}
