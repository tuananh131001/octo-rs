//! `SubsonicController.PlaylistNames` (L729): the playlist names in a relayed getPlaylists
//! body. Used by getPlaylists' library-action playlist provisioning (6-A1's route).

use octo_core::json::dom::Node;
use octo_subsonic::xml::XElement;

/// The playlist names in a relayed getPlaylists body, so provisioning can tell what is
/// already there without a second request. Best-effort: an unreadable body simply means
/// nothing is known to exist, and creating a duplicate is refused by Navidrome anyway.
#[allow(dead_code)] // called by getPlaylists (6-A1)
pub(crate) fn playlist_names(body: Option<&[u8]>, format: &str) -> Vec<String> {
    let Some(body) = body.filter(|b| !b.is_empty()) else {
        return Vec::new();
    };
    // XML too: Navidrome does not refuse a second playlist with the same name, so a client
    // that asks for XML used to get every action playlist created again on each boot.
    if !format.eq_ignore_ascii_case("json") {
        let Ok(root) = XElement::parse(&String::from_utf8_lossy(body)) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        collect_playlist_names(&root, &mut names);
        return names;
    }
    json_playlist_names(body).unwrap_or_default()
}

/// `Descendants().Where(LocalName == "playlist")`: in document order, the root itself excluded.
fn collect_playlist_names(element: &XElement, names: &mut Vec<String>) {
    for child in element.elements() {
        if local_name(&child.name) == "playlist"
            && let Some(name) = child.attribute("name").filter(|n| !n.is_empty())
        {
            names.push(name.to_string());
        }
        collect_playlist_names(child, names);
    }
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// The JSON branch. `None` for anything the C# threw on (caught as "nothing known").
fn json_playlist_names(body: &[u8]) -> Option<Vec<String>> {
    let root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
    // JsonNode's indexer threw on anything but an object (or a missing/null node).
    let index = |node: Option<&Node>, key: &str| -> Option<Option<Node>> {
        match node {
            None | Some(Node::Null) => Some(None),
            Some(Node::Object(fields)) => Some(fields.get(key).cloned()),
            Some(_) => None,
        }
    };
    let response = index(Some(&root), "subsonic-response")?;
    let playlists = index(response.as_ref(), "playlists")?;
    let rows = index(playlists.as_ref(), "playlist")?;
    let Some(Node::Array(rows)) = rows else {
        return Some(Vec::new());
    };
    let mut names = Vec::new();
    for row in &rows {
        match index(Some(row), "name")? {
            None | Some(Node::Null) => {}
            Some(Node::String(name)) if !name.is_empty() => names.push(name),
            Some(Node::String(_)) => {}
            // GetValue<string> on anything else threw.
            Some(_) => return None,
        }
    }
    Some(names)
}
