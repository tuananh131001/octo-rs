//! Port of `Services/Library/GeneratedPlaylistService.cs`: only the mix record.
//!
//! STUB(5-C): replaced when 5-C (Last.fm radio and generated playlists) lands with the service.

use chrono::{DateTime, Utc};

/// One genre or decade mix built from the listener's own library (#54), served like a radio
/// station under an id that starts "og".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeneratedPlaylist {
    pub id: String,
    pub key: String,
    pub kind: String,
    pub label: String,
    pub name: String,
    pub owner: String,
    pub pool_size: i32,
    pub period_start_utc: DateTime<Utc>,
    pub period_end_utc: DateTime<Utc>,
}
