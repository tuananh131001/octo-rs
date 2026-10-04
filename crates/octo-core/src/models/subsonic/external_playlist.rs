//! Port of `Models/Subsonic/ExternalPlaylist.cs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::json::datetime;

/// Represents a playlist from an external music provider (Deezer, Qobuz).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ExternalPlaylist {
    /// Unique identifier in the format "pl-{provider}-{externalId}"
    /// Example: "pl-deezer-123456" or "pl-qobuz-789"
    pub id: String,

    /// Playlist name
    pub name: String,

    /// Playlist description
    pub description: Option<String>,

    /// Name of the playlist creator/curator
    pub curator_name: Option<String>,

    /// Provider name ("deezer" or "qobuz")
    pub provider: String,

    /// External ID from the provider (without "pl-" prefix)
    pub external_id: String,

    /// Number of tracks in the playlist
    pub track_count: i32,

    /// Total duration in seconds
    pub duration: i32,

    /// Cover art URL from the provider
    pub cover_url: Option<String>,

    /// Playlist creation date
    #[serde(with = "datetime::utc_option")]
    pub created_date: Option<DateTime<Utc>>,
}
