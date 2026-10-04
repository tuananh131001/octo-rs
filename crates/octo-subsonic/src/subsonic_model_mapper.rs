//! STUB(3-A): replaced when 3-A (Subsonic wire) lands with the full port of
//! `Services/Subsonic/SubsonicModelMapper.cs`.
//!
//! Only the two dedup helpers `SearchSongOrder` needs (3-E) are here, ported as the C# wrote
//! them, over a stand-in for the rows `ParseSearchResponse` returns (`List<object>` holding a
//! `Dictionary<string, object>` per JSON row or an `XElement` per XML row).

use std::collections::HashSet;

use octo_core::common::SongIdentity;
use octo_core::models::domain::Song;

use crate::xml::XElement;

/// One library row as `ParseSearchResponse` returns it.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchRow {
    Json(serde_json::Map<String, serde_json::Value>),
    Xml(XElement),
}

/// Dedup keys for library rows as `ParseSearchResponse` returns them, JSON or XML. The same
/// keys the merge uses to leave out an outside song you already own.
pub fn local_song_keys(local_songs: &[SearchRow]) -> HashSet<String> {
    let mut keys = HashSet::new();
    for song in local_songs {
        let key = match song {
            SearchRow::Json(dict) => {
                song_key(text(dict, "artist").as_deref(), text(dict, "title").as_deref())
            }
            SearchRow::Xml(element) => song_key(attribute(element, "artist"), attribute(element, "title")),
        };
        if let Some(key) = key {
            keys.insert(key);
        }
    }
    keys
}

/// True when `song` is one of the library rows behind `keys`.
pub fn is_listed(song: &Song, keys: &HashSet<String>) -> bool {
    song_key(Some(&song.artist), Some(&song.title)).is_some_and(|key| keys.contains(&key))
}

/// Dedup key for a song: one song in one version, however its artist and title are written
/// ("Drake feat. Rihanna" or "Too Good (feat. Rihanna)"). A live take or a remix keeps its own.
/// `None` without both an artist and a title.
fn song_key(artist: Option<&str>, title: Option<&str>) -> Option<String> {
    let (artist, title) = (artist.unwrap_or(""), title.unwrap_or(""));
    if SongIdentity::key(artist).is_empty() || SongIdentity::key(title).is_empty() {
        return None;
    }
    Some(SongIdentity::match_key(artist, title))
}

fn text(dict: &serde_json::Map<String, serde_json::Value>, name: &str) -> Option<String> {
    match dict.get(name)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn attribute<'a>(element: &'a XElement, name: &str) -> Option<&'a str> {
    element
        .attributes
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}
