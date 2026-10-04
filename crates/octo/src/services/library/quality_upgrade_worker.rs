//! STUB(5-B): replaced when 5-B (queues and sweeps) lands with the port of
//! `Services/Library/QualityUpgradeWorker.cs`. Only `LibrarySongRow` and the parser
//! `NavidromePlaylistApi::list_songs` (3-E) uses are here, ported as the C# wrote them.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySongRow {
    pub id: String,
    pub path: String,
    pub library_path: Option<String>,
    pub size: i64,
    pub suffix: String,
    pub bit_rate: i32,
    pub title: String,
    pub artist: String,
    pub duration: Option<i32>,
    pub album: Option<String>,
}

/// The rows of one page of Navidrome's native song list, with how many it held, or `None` when
/// it is not a list. `Err` where the C# threw (a size or bit rate that is not a number).
pub fn parse_songs(root: &Value) -> Result<Option<(Vec<LibrarySongRow>, usize)>, String> {
    let Value::Array(songs) = root else {
        return Ok(None);
    };
    let mut rows = Vec::new();
    let mut count = 0;
    for song in songs {
        count += 1;
        if song.get("missing") == Some(&Value::Bool(true)) {
            continue;
        }
        let (Some(id), Some(path)) = (text(song, "id"), text(song, "path")) else {
            continue;
        };
        if id.is_empty() || path.is_empty() {
            continue;
        }
        rows.push(LibrarySongRow {
            id,
            path,
            library_path: text(song, "libraryPath"),
            size: number(song, "size")?.and_then(|n| n.as_i64()).unwrap_or(0),
            suffix: text(song, "suffix").unwrap_or_default(),
            bit_rate: number(song, "bitRate")?
                .and_then(|n| n.as_i64())
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0),
            title: text(song, "title").unwrap_or_default(),
            artist: text(song, "artist").unwrap_or_default(),
            // A float in Navidrome's native API, unlike Subsonic's whole seconds.
            duration: match song.get("duration") {
                Some(Value::Number(n)) => n.as_f64().map(|d| octo_core::common::dotnet::round(d, 0) as i32),
                _ => None,
            },
            album: text(song, "album"),
        });
    }
    Ok(Some((rows, count)))
}

fn text(element: &Value, name: &str) -> Option<String> {
    match element.get(name)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// `TryGetInt64`/`TryGetInt32` on a member: absent is fine, a number is read, and any other
/// kind threw `InvalidOperationException`.
fn number<'a>(element: &'a Value, name: &str) -> Result<Option<&'a serde_json::Number>, String> {
    match element.get(name) {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n)),
        Some(_) => Err(format!(
            "The requested operation requires an element of type 'Number' ({name})."
        )),
    }
}
