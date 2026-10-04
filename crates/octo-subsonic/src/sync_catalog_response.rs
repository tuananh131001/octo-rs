//! Port of `Services/Subsonic/SyncCatalogResponse.cs`.
//!
//! Reads and extends a relayed search2/search3 page for a sync walk. Navidrome's page goes
//! out byte-for-byte apart from the catalog rows added after its own, so nothing about the
//! library rows a syncing client already has can change because this ran.

use std::collections::HashMap;

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use octo_core::json::dom::Node;
use octo_core::models::domain::{Album, Artist, Song};
use serde_json::Value;

use crate::subsonic_response_builder::{Fields, SubsonicResponseBuilder, millis, to_node};
use crate::xml::{XElement, XNode};

/// How many artists, albums and songs a page holds. None when it cannot be read, which
/// leaves the page alone.
pub fn count_rows(body: &[u8], content_type: Option<&str>, envelope: &str) -> Option<(usize, usize, usize)> {
    if is_json(content_type) {
        let document: Value = serde_json::from_slice(body).ok()?;
        let response = document.as_object()?.get("subsonic-response")?;
        let response = response.as_object()?;
        let Some(result) = response.get(envelope) else {
            return Some((0, 0, 0));
        };
        let result = result.as_object()?;
        let count = |name: &str| match result.get(name) {
            Some(Value::Array(rows)) => rows.len(),
            _ => 0,
        };
        return Some((count("artist"), count("album"), count("song")));
    }

    let root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let Some(element) = root.elements().find(|child| local_name(child) == envelope) else {
        return Some((0, 0, 0));
    };
    let count = |name: &str| {
        element
            .elements()
            .filter(|child| local_name(child) == name)
            .count()
    };
    Some((count("artist"), count("album"), count("song")))
}

/// The page with catalog rows appended after the library's, each kind after its own. The
/// builder renders the rows, so they are the same shape every other injected row has;
/// `created` is then set to when the row joined the catalog. An outside song has no
/// date of its own (it is not a file), so it gets this one, which is true of the catalog.
///
/// `added` is the catalog's `Added`: when each id joined it. A page that does not read as the
/// content type says is an error (C# threw), which the caller only meets after
/// [`count_rows`] read the same page.
pub fn append(
    body: &[u8],
    content_type: Option<&str>,
    envelope: &str,
    builder: &SubsonicResponseBuilder,
    added: &HashMap<String, DateTime<Utc>>,
    artists: &[Artist],
    albums: &[Album],
    songs: &[Song],
) -> anyhow::Result<Vec<u8>> {
    let created = |id: &str| added.get(id).map(millis);

    if is_json(content_type) {
        let text = std::str::from_utf8(body).context("the page is not UTF-8")?;
        let mut root = Node::parse(text).map_err(|e| anyhow!("the page is not JSON: {e}"))?;
        let response = root
            .as_object_mut()
            .context("the page is not a JSON object")?
            .get_mut("subsonic-response")
            .and_then(Node::as_object_mut)
            .context("the page has no subsonic-response object")?;
        if !matches!(response.get(envelope), Some(Node::Object(_))) {
            response.insert(envelope.to_string(), Node::object());
        }
        let Some(Node::Object(result)) = response.get_mut(envelope) else {
            unreachable!("the envelope was just made an object");
        };

        let mut add_rows = |name: &str, rows: Vec<(Fields, &str, bool)>| {
            if rows.is_empty() {
                return;
            }
            if !matches!(result.get(name), Some(Node::Array(_))) {
                result.insert(name.to_string(), Node::Array(Vec::new()));
            }
            let Some(Node::Array(list)) = result.get_mut(name) else {
                unreachable!("the row list was just made an array");
            };
            for (fields, id, dated) in rows {
                let Node::Object(mut node) = to_node(&Value::Object(fields)) else {
                    unreachable!("an object converts to an object");
                };
                if let Some(created) = created(id)
                    && (dated || node.contains_key("created"))
                {
                    node.insert("created".to_string(), Node::String(created));
                }
                list.push(Node::Object(node));
            }
        };

        add_rows(
            "artist",
            artists
                .iter()
                .map(|artist| (builder.convert_artist_to_json(artist), artist.id.as_str(), false))
                .collect(),
        );
        add_rows(
            "album",
            albums
                .iter()
                .map(|album| (builder.convert_album_to_json(album), album.id.as_str(), false))
                .collect(),
        );
        add_rows(
            "song",
            songs
                .iter()
                .map(|song| (builder.convert_song_to_json(song), song.id.as_str(), true))
                .collect(),
        );
        return Ok(root.to_json_string(false).into_bytes());
    }

    let mut document =
        XElement::parse(&String::from_utf8_lossy(body)).map_err(|e| anyhow!("the page is not XML: {e}"))?;
    let ns = document.namespace.clone();
    let container_index = match document
        .content
        .iter()
        .position(|node| matches!(node, XNode::Element(child) if local_name(child) == envelope))
    {
        Some(index) => index,
        None => {
            document.push(XElement::in_namespace(ns.as_deref(), envelope));
            document.content.len() - 1
        }
    };
    let XNode::Element(container) = &mut document.content[container_index] else {
        unreachable!("the container is an element");
    };

    let stamp = |mut element: XElement, id: &str, dated: bool| {
        if let Some(created) = created(id)
            && (dated || element.attribute("created").is_some())
        {
            element.set_attr("created", created);
        }
        element
    };

    // Kept in schema order (artists, then albums, then songs) for clients that validate it.
    let insert = |container: &mut XElement, kind: &str, elements: Vec<XElement>, after: &[&str]| {
        if elements.is_empty() {
            return;
        }
        let anchor = container.content.iter().rposition(|node| {
            matches!(node, XNode::Element(child)
                if local_name(child) == kind || after.contains(&local_name(child)))
        });
        match anchor {
            None => container.add_first(elements),
            Some(index) => container.insert_after(index, elements),
        }
    };

    let ns = ns.as_deref();
    insert(
        container,
        "artist",
        artists
            .iter()
            .map(|artist| stamp(builder.convert_artist_to_xml(artist, ns), &artist.id, false))
            .collect(),
        &[],
    );
    insert(
        container,
        "album",
        albums
            .iter()
            .map(|album| stamp(builder.convert_album_to_xml(album, ns), &album.id, false))
            .collect(),
        &["artist"],
    );
    insert(
        container,
        "song",
        songs
            .iter()
            .map(|song| stamp(builder.convert_song_to_xml(song, ns), &song.id, true))
            .collect(),
        &["artist", "album"],
    );
    Ok(document.to_xml_string().into_bytes())
}

fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|c| c.to_ascii_lowercase().contains("json"))
}

/// `Name.LocalName`: the name after any prefix.
fn local_name(element: &XElement) -> &str {
    element.name.rsplit(':').next().unwrap_or(&element.name)
}

#[cfg(test)]
#[path = "sync_catalog_response_tests.rs"]
mod tests;
