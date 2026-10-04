//! STUB(5-A): replaced when 5-A (library actions) lands with the port of
//! `Services/Library/LibraryActionPlaylistWorker.cs`. Only the row types and parsers
//! `NavidromePlaylistApi` (3-E) returns are here, ported as the C# wrote them.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRow {
    pub id: String,
    pub name: String,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTrackRow {
    pub position: String,
    pub media_file_id: String,
}

pub fn parse_playlists(root: &Value) -> Vec<PlaylistRow> {
    let Value::Array(items) = root else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for item in items {
        let (Some(id), Some(name)) = (text(item, "id"), text(item, "name")) else {
            continue;
        };
        if id.is_empty() || name.is_empty() {
            continue;
        }
        rows.push(PlaylistRow {
            id,
            name,
            owner: text(item, "ownerName").unwrap_or_default(),
        });
    }
    rows
}

pub fn parse_tracks(root: &Value) -> Vec<PlaylistTrackRow> {
    let Value::Array(items) = root else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for item in items {
        let Some(media_file_id) = text(item, "mediaFileId").filter(|m| !m.is_empty()) else {
            continue;
        };
        rows.push(PlaylistTrackRow {
            position: text(item, "id").unwrap_or_default(),
            media_file_id,
        });
    }
    rows
}

/// A string member, or a number's text; anything else is missing.
fn text(element: &Value, name: &str) -> Option<String> {
    match element.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
