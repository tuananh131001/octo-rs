//! Port of `Services/Subsonic/SubsonicModelMapper.cs`: parsing Subsonic search responses and
//! merging local with external search results.

use std::collections::HashSet;

use octo_core::common::SongIdentity;
use octo_core::models::domain::Song;
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use serde_json::Value;
use tracing::warn;

use crate::subsonic_response_builder::{
    Fields, SUBSONIC_NAMESPACE, SubsonicResponseBuilder, curator_artist_id, curator_artist_name, text_of,
};
use crate::xml::XElement;

/// One library row as the C# held it in a `List<object>`: a `Dictionary<string, object>`
/// read from Navidrome's JSON, or an `XElement` read from its XML.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Json(Value),
    Xml(XElement),
}

impl Row {
    /// The JSON row, if this is one.
    pub fn as_json(&self) -> Option<&Value> {
        match self {
            Row::Json(value) => Some(value),
            Row::Xml(_) => None,
        }
    }

    /// The XML row, if this is one.
    pub fn as_xml(&self) -> Option<&XElement> {
        match self {
            Row::Xml(element) => Some(element),
            Row::Json(_) => None,
        }
    }

    pub fn into_json(self) -> Option<Value> {
        match self {
            Row::Json(value) => Some(value),
            Row::Xml(_) => None,
        }
    }

    pub fn into_xml(self) -> Option<XElement> {
        match self {
            Row::Xml(element) => Some(element),
            Row::Json(_) => None,
        }
    }
}

/// Songs, albums and artists, in that order, as C#'s tuples held them.
pub type SearchRows = (Vec<Row>, Vec<Row>, Vec<Row>);

/// Handles parsing Subsonic API responses and merging local with external search results.
#[derive(Debug, Clone)]
pub struct SubsonicModelMapper {
    response_builder: SubsonicResponseBuilder,
}

impl SubsonicModelMapper {
    pub fn new(response_builder: SubsonicResponseBuilder) -> Self {
        Self { response_builder }
    }

    /// Parses a Subsonic search response and extracts songs, albums, and artists.
    ///
    /// A body it cannot read is logged and gives whatever rows were read before the problem,
    /// as the C# did by catching around the whole read.
    pub fn parse_search_response(&self, response_body: &[u8], content_type: Option<&str>) -> SearchRows {
        let mut rows: SearchRows = (Vec::new(), Vec::new(), Vec::new());
        if let Err(e) = self.read_search_response(response_body, content_type, &mut rows) {
            warn!("Error parsing Subsonic search response: {e}");
        }
        rows
    }

    fn read_search_response(
        &self,
        response_body: &[u8],
        content_type: Option<&str>,
        (songs, albums, artists): &mut SearchRows,
    ) -> Result<(), String> {
        let content = String::from_utf8_lossy(response_body);

        if content_type.is_some_and(|c| c.contains("json")) {
            let document: Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;
            let root = document.as_object().ok_or("the JSON root is not an object")?;
            // Both envelopes: search2 and search3 are the same hijack, and a search2
            // relay answers under searchResult2. Reading only searchResult3 silently
            // dropped every local row for search2 clients.
            let Some(response) = root.get("subsonic-response") else {
                return Ok(());
            };
            let response = response.as_object().ok_or("subsonic-response is not an object")?;
            let Some(search_result) = response
                .get("searchResult3")
                .or_else(|| response.get("searchResult2"))
            else {
                return Ok(());
            };
            let search_result = search_result
                .as_object()
                .ok_or("the search result is not an object")?;
            for (name, list) in [("song", songs), ("album", albums), ("artist", artists)] {
                let Some(elements) = search_result.get(name) else {
                    continue;
                };
                let elements = elements.as_array().ok_or("a row list is not an array")?;
                for element in elements {
                    let row = self
                        .response_builder
                        .convert_subsonic_json_element(element, true)
                        .ok_or("a row is not an object")?;
                    list.push(Row::Json(row));
                }
            }
        } else {
            let root = XElement::parse(&content).map_err(|e| e.to_string())?;
            let ns = root.namespace.as_deref();
            // Descendants of the document: the root itself counts.
            let find = |name: &str| {
                if root.is(ns, name) {
                    Some(&root)
                } else {
                    root.first_descendant(ns, name)
                }
            };
            if let Some(search_result) = find("searchResult3").or_else(|| find("searchResult2")) {
                for song in search_result.elements_named(ns, "song") {
                    songs.push(Row::Xml(
                        self.response_builder.convert_subsonic_xml_element(song, "song"),
                    ));
                }
                for album in search_result.elements_named(ns, "album") {
                    albums.push(Row::Xml(
                        self.response_builder.convert_subsonic_xml_element(album, "album"),
                    ));
                }
                for artist in search_result.elements_named(ns, "artist") {
                    artists.push(Row::Xml(
                        self.response_builder
                            .convert_subsonic_xml_element(artist, "artist"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Merges local and external search results (songs, albums, artists, playlists).
    ///
    /// `trailing_local_songs`: library rows that follow the outside ones. Only a later page of
    /// a search has any: there the rest of the library comes after the outside songs page one
    /// started.
    pub fn merge_search_results(
        &self,
        local_songs: Vec<Row>,
        local_albums: Vec<Row>,
        local_artists: Vec<Row>,
        external_result: &SearchResult,
        external_playlists: &[ExternalPlaylist],
        is_json: bool,
        trailing_local_songs: Option<Vec<Row>>,
    ) -> SearchRows {
        let trailing = trailing_local_songs.unwrap_or_default();
        if is_json {
            self.merge_search_results_json(
                local_songs,
                local_albums,
                local_artists,
                external_result,
                external_playlists,
                trailing,
            )
        } else {
            self.merge_search_results_xml(
                local_songs,
                local_albums,
                local_artists,
                external_result,
                external_playlists,
                trailing,
            )
        }
    }

    /// Dedup keys for library rows as [`Self::parse_search_response`] returns them, JSON or
    /// XML. The same keys the merge uses to leave out an outside song you already own.
    pub fn local_song_keys<'a>(local_songs: impl IntoIterator<Item = &'a Row>) -> HashSet<String> {
        local_songs
            .into_iter()
            .filter_map(|song| match song {
                Row::Json(Value::Object(dict)) => {
                    song_key(text(dict, "artist").as_deref(), text(dict, "title").as_deref())
                }
                Row::Json(_) => None,
                Row::Xml(element) => song_key(element.attribute("artist"), element.attribute("title")),
            })
            .collect()
    }

    /// True when `song` is one of the library rows behind `keys`.
    pub fn is_listed(song: &Song, keys: &HashSet<String>) -> bool {
        song_key(Some(&song.artist), Some(&song.title)).is_some_and(|key| keys.contains(&key))
    }

    fn merge_search_results_json(
        &self,
        local_songs: Vec<Row>,
        local_albums: Vec<Row>,
        local_artists: Vec<Row>,
        external_result: &SearchResult,
        external_playlists: &[ExternalPlaylist],
        trailing_local_songs: Vec<Row>,
    ) -> SearchRows {
        // Local songs first, external (YouTube placeholder) after. The earlier
        // version flipped this to put externals first because Arpeggi's "play
        // artist radio" feature reused search3 with songCount=2000 — locals
        // first would crowd externals out of its top-N. Now Arpeggi/Narjo
        // radio goes through getSimilarSongs2, so search3 is plain search and
        // users expect their owned tracks to top the results, with discovery
        // suggestions following.
        // An outside song you already own is not listed again under it.
        let local_song_keys: HashSet<String> = local_songs
            .iter()
            .chain(trailing_local_songs.iter())
            .filter_map(|row| match row {
                Row::Json(Value::Object(dict)) => {
                    song_key(text(dict, "artist").as_deref(), text(dict, "title").as_deref())
                }
                _ => None,
            })
            .collect();
        let mut merged_songs = local_songs;
        merged_songs.extend(
            external_result
                .songs
                .iter()
                .filter(|s| {
                    song_key(Some(&s.artist), Some(&s.title)).is_none_or(|k| !local_song_keys.contains(&k))
                })
                .map(|s| Row::Json(Value::Object(self.response_builder.convert_song_to_json(s)))),
        );
        merged_songs.extend(trailing_local_songs);

        // Albums, deduplicated by artist+name so an album you own is not listed twice.
        // Playlists follow, appearing as albums with genre "Playlist".
        let mut local_album_keys = HashSet::new();
        for album in &local_albums {
            let Row::Json(Value::Object(dict)) = album else {
                continue;
            };
            if let Some(key) = album_key(text(dict, "artist").as_deref(), text(dict, "name").as_deref()) {
                local_album_keys.insert(key);
            }
        }

        let mut merged_albums = local_albums;
        merged_albums.extend(
            external_result
                .albums
                .iter()
                .filter(|a| {
                    album_key(Some(&a.artist), Some(&a.title)).is_none_or(|k| !local_album_keys.contains(&k))
                })
                .map(|a| Row::Json(Value::Object(self.response_builder.convert_album_to_json(a)))),
        );
        merged_albums.extend(
            external_playlists
                .iter()
                .map(|p| Row::Json(Value::Object(convert_playlist_to_album_json(p)))),
        );

        // Deduplicate artists by name - prefer local artists over external ones
        let mut local_artist_names = HashSet::new();
        for artist in &local_artists {
            if let Row::Json(Value::Object(dict)) = artist
                && dict.contains_key("name")
            {
                local_artist_names.insert(SongIdentity::key(text(dict, "name").as_deref().unwrap_or("")));
            }
        }

        let mut merged_artists = local_artists;
        for external_artist in &external_result.artists {
            // Only add external artist if no local artist with same name exists
            if !local_artist_names.contains(&SongIdentity::key(&external_artist.name)) {
                merged_artists.push(Row::Json(Value::Object(
                    self.response_builder.convert_artist_to_json(external_artist),
                )));
            }
        }

        (merged_songs, merged_albums, merged_artists)
    }

    /// The XML merge. Every library row here is an element: the C# cast them, and a JSON row
    /// cannot reach it (rows are read as JSON exactly when the merge is JSON), so one is skipped.
    fn merge_search_results_xml(
        &self,
        local_songs: Vec<Row>,
        local_albums: Vec<Row>,
        local_artists: Vec<Row>,
        external_result: &SearchResult,
        external_playlists: &[ExternalPlaylist],
        trailing_local_songs: Vec<Row>,
    ) -> SearchRows {
        let ns = Some(SUBSONIC_NAMESPACE);
        let rename = |mut element: XElement, name: &str| {
            element.name = name.to_string();
            element.namespace = ns.map(str::to_string);
            element
        };

        // Deduplicate artists by name - prefer local artists over external ones
        let mut local_artist_names = HashSet::new();
        let mut merged_artists = Vec::new();

        for artist in local_artists.into_iter().filter_map(Row::into_xml) {
            if let Some(name) = artist.attribute("name").filter(|n| !n.is_empty()) {
                local_artist_names.insert(SongIdentity::key(name));
            }
            merged_artists.push(Row::Xml(rename(artist, "artist")));
        }

        for artist in &external_result.artists {
            // Only add external artist if no local artist with same name exists
            if !local_artist_names.contains(&SongIdentity::key(&artist.name)) {
                merged_artists.push(Row::Xml(self.response_builder.convert_artist_to_xml(artist, ns)));
            }
        }

        // Albums, deduplicated by artist+name so an album you own is not listed twice.
        let mut local_album_keys = HashSet::new();
        let mut merged_albums = Vec::new();
        for album in local_albums.into_iter().filter_map(Row::into_xml) {
            if let Some(key) = album_key(album.attribute("artist"), album.attribute("name")) {
                local_album_keys.insert(key);
            }
            merged_albums.push(Row::Xml(rename(album, "album")));
        }
        for album in &external_result.albums {
            if album_key(Some(&album.artist), Some(&album.title))
                .is_some_and(|key| local_album_keys.contains(&key))
            {
                continue;
            }
            merged_albums.push(Row::Xml(self.response_builder.convert_album_to_xml(album, ns)));
        }
        // Add playlists as albums
        for playlist in external_playlists {
            merged_albums.push(Row::Xml(convert_playlist_to_album_xml(playlist, ns)));
        }

        // Songs, without an outside song you already own
        let mut merged_songs = Vec::new();
        let mut local_song_keys = HashSet::new();
        for song in local_songs.into_iter().filter_map(Row::into_xml) {
            if let Some(key) = song_key(song.attribute("artist"), song.attribute("title")) {
                local_song_keys.insert(key);
            }
            merged_songs.push(Row::Xml(rename(song, "song")));
        }
        let trailing: Vec<XElement> = trailing_local_songs
            .into_iter()
            .filter_map(Row::into_xml)
            .collect();
        for song in &trailing {
            if let Some(key) = song_key(song.attribute("artist"), song.attribute("title")) {
                local_song_keys.insert(key);
            }
        }
        for song in &external_result.songs {
            if song_key(Some(&song.artist), Some(&song.title))
                .is_some_and(|key| local_song_keys.contains(&key))
            {
                continue;
            }
            merged_songs.push(Row::Xml(self.response_builder.convert_song_to_xml(song, ns)));
        }
        for song in trailing {
            merged_songs.push(Row::Xml(rename(song, "song")));
        }

        (merged_songs, merged_albums, merged_artists)
    }
}

/// Dedup key for an album, case, accents and punctuation ignored. None when there is not
/// enough to compare on, which means "never treat this as a duplicate". Shared with the native
/// album search, so both APIs agree on what counts as the same album.
pub fn album_key(artist: Option<&str>, name: Option<&str>) -> Option<String> {
    let name = name.filter(|n| !n.trim().is_empty())?;
    Some(format!(
        "{}|{}",
        SongIdentity::key(artist.unwrap_or("")),
        SongIdentity::key(name)
    ))
}

/// Dedup key for a song: one song in one version, however its artist and title are
/// written ("Drake feat. Rihanna" or "Too Good (feat. Rihanna)"). A live take or a remix
/// keeps its own. None without both an artist and a title.
fn song_key(artist: Option<&str>, title: Option<&str>) -> Option<String> {
    let artist = artist.unwrap_or("");
    let title = title.unwrap_or("");
    if SongIdentity::key(artist).is_empty() || SongIdentity::key(title).is_empty() {
        None
    } else {
        Some(SongIdentity::match_key(artist, title))
    }
}

fn text(dict: &Fields, name: &str) -> Option<String> {
    text_of(dict.get(name))
}

/// Converts an ExternalPlaylist to a JSON object representing an album.
/// Playlists are represented as albums with genre "Playlist" and artist "🎵 {Provider} {Curator}".
fn convert_playlist_to_album_json(playlist: &ExternalPlaylist) -> Fields {
    let mut album = Fields::new();
    album.insert("id".into(), playlist.id.clone().into());
    album.insert("name".into(), playlist.name.clone().into());
    album.insert("artist".into(), curator_artist_name(playlist).into());
    album.insert("artistId".into(), curator_artist_id(playlist).into());
    album.insert("genre".into(), "Playlist".into());
    album.insert("songCount".into(), playlist.track_count.into());
    album.insert("duration".into(), playlist.duration.into());

    if let Some(created) = &playlist.created_date {
        album.insert("year".into(), chrono::Datelike::year(created).into());
        album.insert(
            "created".into(),
            created.format("%Y-%m-%dT%H:%M:%S").to_string().into(),
        );
    }

    if playlist.cover_url.as_deref().is_some_and(|url| !url.is_empty()) {
        album.insert("coverArt".into(), playlist.id.clone().into());
    }

    album
}

/// Converts an ExternalPlaylist to an XML element representing an album.
/// Playlists are represented as albums with genre "Playlist" and artist "🎵 {Provider} {Curator}".
fn convert_playlist_to_album_xml(playlist: &ExternalPlaylist, ns: Option<&str>) -> XElement {
    let mut album = XElement::in_namespace(ns, "album")
        .attr("id", &playlist.id)
        .attr("name", &playlist.name)
        .attr("artist", curator_artist_name(playlist))
        .attr("artistId", curator_artist_id(playlist))
        .attr("genre", "Playlist")
        .attr("songCount", playlist.track_count)
        .attr("duration", playlist.duration);

    if let Some(created) = &playlist.created_date {
        album.set_attr("year", chrono::Datelike::year(created));
        album.set_attr("created", created.format("%Y-%m-%dT%H:%M:%S").to_string());
    }

    if playlist.cover_url.as_deref().is_some_and(|url| !url.is_empty()) {
        album.set_attr("coverArt", &playlist.id);
    }

    album
}

#[cfg(test)]
#[path = "subsonic_model_mapper_tests.rs"]
mod tests;
