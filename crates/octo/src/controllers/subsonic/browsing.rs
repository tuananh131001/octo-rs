//! `getSong`, `getArtist` and `getAlbum` (`SubsonicController.GetSong` L1655, `GetArtist`
//! L1691, `GetAlbum` L1837; endpoints.md §3.2): an outside id is answered from the catalog, a
//! library artist or album is Navidrome's merged with what the catalog has that the library
//! lacks.

use std::collections::HashSet;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::common::dotnet::{self, is_blank};
use octo_core::common::playlist_id_helper;
use octo_core::common::{AlbumFillIn, LibraryTrack, SongIdentity};
use octo_core::models::domain::Album;
use serde_json::Value;
use tracing::{debug, error};

use super::helpers_6a2::{SubsonicCall, as_json, file};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::i_music_metadata_service::IMusicMetadataService;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("getSong", get(get_song).post(get_song))
        .subsonic("getArtist", get(get_artist).post(get_artist))
        .subsonic("getAlbum", get(get_album).post(get_album))
}

/// Returns external song info if needed.
pub async fn get_song(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let builder = &state.subsonic_response_builder;

    if is_blank(&id) {
        return Ok(builder.create_error(&format, 10, "Missing id parameter").into_response());
    }

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);

    if !is_external {
        // Exceptions are not caught here: they reach the global handler.
        let result = call.proxy.relay("rest/getSong", &call.parameters).await?;
        let content_type = result
            .content_type
            .unwrap_or_else(|| format!("application/{format}"));
        return Ok(file(result.body, &content_type));
    }

    let song = state
        .music_metadata
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await;

    let Some(song) = song else {
        return Ok(builder.create_error(&format, 70, "Song not found").into_response());
    };

    Ok(builder.create_song_response(&format, &song).into_response())
}

/// Merges local and Deezer albums.
pub async fn get_artist(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let builder = &state.subsonic_response_builder;
    let metadata = &state.music_metadata;

    if is_blank(&id) {
        return Ok(builder.create_error(&format, 10, "Missing id parameter").into_response());
    }

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);

    if is_external {
        let provider = provider.unwrap_or_default();
        let external_id = external_id.unwrap_or_default();
        let Some(artist) = metadata.get_artist(&provider, &external_id).await else {
            return Ok(builder.create_error(&format, 70, "Artist not found").into_response());
        };

        let mut albums = metadata.get_artist_albums(&provider, &external_id).await;

        // Fill artist info for each album (Deezer API doesn't include it in artist/albums endpoint)
        for album in &mut albums {
            if album.artist.is_empty() {
                album.artist = artist.name.clone();
            }
            if album.artist_id.as_deref().unwrap_or("").is_empty() {
                album.artist_id = Some(artist.id.clone());
            }
        }

        return Ok(builder.create_artist_response(&format, &artist, &albums).into_response());
    }

    // Merged from Navidrome's JSON whatever the client asked for, then answered in the
    // client's format: see CreateMergedResponse.
    let Some(navidrome) = call.proxy.relay_safe("rest/getArtist", &as_json(&call.parameters)).await else {
        return Ok(builder.create_error(&format, 70, "Artist not found").into_response());
    };

    let mut artist_name = String::new();
    let local_artist_id = id.clone(); // Keep the local artist ID for merged albums
    let mut local_albums: Vec<Value> = Vec::new();
    let mut artist_data: Option<Value> = None;

    if navidrome.content_type.as_deref().is_some_and(|ct| ct.contains("json")) {
        let document = parse_json(&navidrome.body)?;
        if let Some(artist_element) = property(property(Some(&document), "subsonic-response")?, "artist")? {
            artist_name = string_property(artist_element, "name")?;
            artist_data = Some(convert_element(&state, artist_element)?);

            if let Some(albums) = property(Some(artist_element), "album")? {
                for album in enumerate_array(albums)? {
                    local_albums.push(convert_element(&state, album)?);
                }
            }
        }
    }

    let Some(mut artist_data) = artist_data.filter(|_| !artist_name.is_empty()) else {
        return Ok(relay_as_asked(
            &state,
            &call,
            "rest/getArtist",
            &format,
            &navidrome.body,
            navidrome.content_type.as_deref(),
        )
        .await);
    };

    let local_album_titles: Vec<String> = local_albums
        .iter()
        .filter_map(|album| album.as_object()?.get("name").and_then(to_text))
        .collect();

    // The first hit is not reliably this artist: a bigger act whose name contains this one
    // can come first. Of the few asked for, the one with this exact name is.
    let deezer_artists: Vec<_> = metadata
        .search_artists(&artist_name, 5)
        .await
        .into_iter()
        .filter(|found| SongIdentity::same_artist_name(&found.name, &artist_name))
        .collect();
    let mut deezer_albums: Vec<Album> = Vec::new();

    if let Some(deezer_artist) = deezer_artists.first()
        && SongIdentity::same_artist_name(&deezer_artist.name, &artist_name)
    {
        // The provider must come from the artist found, as for albums: a hardcoded
        // "deezer" never matches the metadata service's name, so this was always empty.
        // The library's own albums go along, to tell two artists of one name apart.
        deezer_albums = metadata
            .get_artist_albums_for_library(
                deezer_artist.external_provider.as_deref().unwrap_or_default(),
                deezer_artist.external_id.as_deref().unwrap_or_default(),
                Some(&local_album_titles),
            )
            .await;

        // Fill artist info for each album (Deezer API doesn't include it in artist/albums endpoint)
        // Use local artist ID and name so albums link back to the local artist
        for album in &mut deezer_albums {
            if album.artist.is_empty() {
                album.artist = artist_name.clone();
            }
            if album.artist_id.as_deref().unwrap_or("").is_empty() {
                album.artist_id = Some(local_artist_id.clone());
            }
        }
    }

    // An owned album is one the library has by the matcher's key, so "Discovery" in the
    // library hides the catalog's "Discovery" however either is spelled or punctuated.
    let local_album_names: HashSet<String> = local_album_titles.iter().map(|t| SongIdentity::key(t)).collect();

    let mut merged_albums = local_albums;
    for deezer_album in &deezer_albums {
        if !local_album_names.contains(&SongIdentity::key(&deezer_album.title)) {
            merged_albums.push(Value::Object(builder.convert_album_to_json(deezer_album)));
        }
    }

    if let Some(artist_dict) = artist_data.as_object_mut() {
        let count = merged_albums.len();
        artist_dict.insert("album".into(), Value::Array(merged_albums));
        artist_dict.insert("albumCount".into(), count.into());
    }

    Ok(builder
        .create_merged_response(&format, "artist", &artist_data)
        .into_response())
}

/// Enriches local albums with Deezer songs.
pub async fn get_album(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let builder = &state.subsonic_response_builder;
    let metadata = &state.music_metadata;

    if is_blank(&id) {
        return Ok(builder.create_error(&format, 10, "Missing id parameter").into_response());
    }

    // Check if this is an external playlist
    if playlist_id_helper::is_external_playlist(Some(&id)) {
        let Ok((provider, external_id)) = playlist_id_helper::parse_playlist_id(&id) else {
            error!("Error getting playlist {id}");
            return Ok(builder.create_error(&format, 70, "Playlist not found").into_response());
        };

        // Get playlist metadata
        let Some(playlist) = metadata.get_playlist(&provider, &external_id).await else {
            return Ok(builder.create_error(&format, 70, "Playlist not found").into_response());
        };

        // Get playlist tracks
        let tracks = metadata.get_playlist_tracks(&provider, &external_id).await;

        // The C# added every track to PlaylistSyncService's playlist cache here, so a played
        // track would be known to belong to this playlist. That service was never registered
        // (its constructor argument was always null), so nothing was ever added.

        // Convert to album response (playlist as album)
        return Ok(builder
            .create_playlist_as_album_response(&format, &playlist, &tracks)
            .into_response());
    }

    let (is_external, album_provider, album_external_id) = state.local_library.parse_song_id(&id);

    if is_external {
        let album = metadata
            .get_album(
                album_provider.as_deref().unwrap_or_default(),
                album_external_id.as_deref().unwrap_or_default(),
            )
            .await;

        let Some(album) = album else {
            return Ok(builder.create_error(&format, 70, "Album not found").into_response());
        };

        return Ok(builder.create_album_response(&format, &album).into_response());
    }

    // Merged from Navidrome's JSON whatever the client asked for, then answered in the
    // client's format: see CreateMergedResponse.
    let Some(navidrome) = call.proxy.relay_safe("rest/getAlbum", &as_json(&call.parameters)).await else {
        return Ok(builder.create_error(&format, 70, "Album not found").into_response());
    };

    let mut album_name = String::new();
    let mut artist_name = String::new();
    let mut local_songs: Vec<Value> = Vec::new();
    let mut album_data: Option<Value> = None;

    if navidrome.content_type.as_deref().is_some_and(|ct| ct.contains("json")) {
        let document = parse_json(&navidrome.body)?;
        if let Some(album_element) = property(property(Some(&document), "subsonic-response")?, "album")? {
            album_name = string_property(album_element, "name")?;
            artist_name = string_property(album_element, "artist")?;
            album_data = Some(convert_element(&state, album_element)?);

            if let Some(songs) = property(Some(album_element), "song")? {
                for song in enumerate_array(songs)? {
                    local_songs.push(convert_element(&state, song)?);
                }
            }
        }
    }

    let Some(mut album_data) = album_data.filter(|_| !album_name.is_empty() && !artist_name.is_empty()) else {
        return Ok(relay_as_asked(
            &state,
            &call,
            "rest/getAlbum",
            &format,
            &navidrome.body,
            navidrome.content_type.as_deref(),
        )
        .await);
    };

    let library: Vec<LibraryTrack> = local_songs
        .iter()
        .map(|song| AlbumFillIn::from_subsonic(Some(song)))
        .collect();

    // The first catalog album by this name that holds the library's songs, known by ISRC
    // or by title and length. A name alone can belong to another record: "Nightcore" by
    // "Nightcore" (octo-player#1).
    let search_query = format!("{artist_name} {album_name}");
    let deezer_albums = metadata.search_albums(&search_query, 5).await;
    let library_songs = i32::try_from(AlbumFillIn::count_songs(&library)).unwrap_or(i32::MAX);
    let mut deezer_album: Option<Album> = None;
    for candidate in AlbumFillIn::candidates(&deezer_albums, &artist_name, &album_name, library_songs) {
        // The provider must come from the candidate. A hardcoded "deezer" never
        // matches the metadata service's provider name, so this always returned null.
        let detail = metadata
            .get_album(
                candidate.external_provider.as_deref().unwrap_or_default(),
                candidate.external_id.as_deref().unwrap_or_default(),
            )
            .await;
        let Some(detail) = detail.filter(|d| !d.songs.is_empty()) else {
            continue;
        };
        if AlbumFillIn::holds(&library, &detail.songs) {
            deezer_album = Some(detail);
            break;
        }
        debug!(
            "getAlbum '{artist_name} - {album_name}': catalog album {} shares the name but not the songs; not filled in from it",
            candidate.external_id.as_deref().unwrap_or("")
        );
    }

    if let Some(deezer_album) = deezer_album {
        let mut merged_songs = local_songs;
        for deezer_song in &deezer_album.songs {
            if !AlbumFillIn::owned(&library, deezer_song) {
                merged_songs.push(Value::Object(builder.convert_song_to_json(deezer_song)));
            }
        }

        // OrderBy: a stable sort on each song's track, 0 for none.
        let mut keyed = Vec::with_capacity(merged_songs.len());
        for song in merged_songs {
            let track = match song.as_object().and_then(|dict| dict.get("track")) {
                Some(track) => convert_to_int32(track)?,
                None => 0,
            };
            keyed.push((track, song));
        }
        keyed.sort_by_key(|(track, _)| *track);
        let merged_songs: Vec<Value> = keyed.into_iter().map(|(_, song)| song).collect();

        if let Some(album_dict) = album_data.as_object_mut() {
            let mut total_duration: i32 = 0;
            for song in &merged_songs {
                if let Some(duration) = song.as_object().and_then(|dict| dict.get("duration")) {
                    total_duration = total_duration.wrapping_add(convert_to_int32(duration)?);
                }
            }
            let count = merged_songs.len();
            album_dict.insert("song".into(), Value::Array(merged_songs));
            album_dict.insert("songCount".into(), count.into());
            album_dict.insert("duration".into(), total_duration.into());
        }
    }

    Ok(builder
        .create_merged_response(&format, "album", &album_data)
        .into_response())
}

/// Navidrome's answer untouched, when there is nothing to merge: the JSON already in hand
/// for a JSON client, otherwise asked again in the client's own format so an XML client
/// never gets JSON.
async fn relay_as_asked(
    state: &AppState,
    call: &SubsonicCall,
    endpoint: &str,
    format: &str,
    json_body: &[u8],
    json_content_type: Option<&str>,
) -> Response {
    if format == "json" {
        return file(json_body.to_vec(), json_content_type.unwrap_or("application/json"));
    }
    match call.proxy.relay_safe(endpoint, &call.parameters).await {
        Some(asked) => file(
            asked.body,
            asked.content_type.as_deref().unwrap_or("application/xml"),
        ),
        None => state
            .subsonic_response_builder
            .create_error(format, 70, "Not found")
            .into_response(),
    }
}

/// `JsonDocument.Parse`: malformed JSON threw a `JsonException`, which the global handler
/// answered as an internal error.
fn parse_json(body: &[u8]) -> Result<Value, AppError> {
    serde_json::from_slice(body)
        .map_err(|e| AppError::Internal(anyhow::Error::new(e).context("Navidrome's answer is not JSON")))
}

/// `TryGetProperty` on an element that may be absent: on anything but an object it threw an
/// `InvalidOperationException`.
fn property<'a>(element: Option<&'a Value>, name: &str) -> Result<Option<&'a Value>, AppError> {
    match element {
        None => Ok(None),
        Some(Value::Object(fields)) => Ok(fields.get(name)),
        Some(_) => Err(AppError::InvalidOperation(format!(
            "The requested operation requires an element of type 'Object' (looking for '{name}')."
        ))),
    }
}

/// `TryGetProperty(name, out var p) ? p.GetString() ?? "" : ""`: GetString on anything but a
/// string or a null threw.
fn string_property(element: &Value, name: &str) -> Result<String, AppError> {
    match property(Some(element), name)? {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(AppError::InvalidOperation(format!(
            "The requested operation requires an element of type 'String' ('{name}')."
        ))),
    }
}

/// `EnumerateArray`, which threw on anything but an array.
fn enumerate_array(element: &Value) -> Result<&Vec<Value>, AppError> {
    element.as_array().ok_or_else(|| {
        AppError::InvalidOperation("The requested operation requires an element of type 'Array'.".into())
    })
}

/// `ConvertSubsonicJsonElement(element, true)`, whose `EnumerateObject` threw on anything but
/// an object.
fn convert_element(state: &AppState, element: &Value) -> Result<Value, AppError> {
    state
        .subsonic_response_builder
        .convert_subsonic_json_element(element, true)
        .ok_or_else(|| {
            AppError::InvalidOperation("The requested operation requires an element of type 'Object'.".into())
        })
}

/// `object?.ToString()` for a converted value: a string as it is, a number as .NET writes it,
/// a bool as `True`/`False`, a null as nothing.
fn to_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(true) => Some("True".into()),
        Value::Bool(false) => Some("False".into()),
        Value::Number(n) => Some(n.to_string()),
        // A list or a dictionary printed its type name.
        Value::Array(_) => Some("System.Collections.Generic.List`1[System.Object]".into()),
        Value::Object(_) => {
            Some("System.Collections.Generic.Dictionary`2[System.String,System.Object]".into())
        }
    }
}

/// `Convert.ToInt32(object)` over a converted JSON value: null is 0, a double is rounded to
/// even, a string is parsed, and anything that cannot be converted threw.
fn convert_to_int32(value: &Value) -> Result<i32, AppError> {
    match value {
        Value::Null => Ok(0),
        Value::Bool(b) => Ok(i32::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return i32::try_from(i)
                    .map_err(|_| AppError::Internal(anyhow::anyhow!("Value was either too large or too small for an Int32.")));
            }
            let x = n.as_f64().unwrap_or(0.0);
            let rounded = dotnet::round(x, 0);
            if rounded > f64::from(i32::MAX) || rounded < f64::from(i32::MIN) || rounded.is_nan() {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Value was either too large or too small for an Int32."
                )));
            }
            Ok(rounded as i32)
        }
        Value::String(text) => text
            .trim()
            .parse::<i32>()
            .map_err(|_| AppError::Format(format!("The input string '{text}' was not in a correct format."))),
        _ => Err(AppError::Internal(anyhow::anyhow!(
            "Unable to cast object to type 'System.IConvertible'."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_to_int32_reads_what_convert_did() {
        assert_eq!(convert_to_int32(&Value::Null).ok(), Some(0));
        assert_eq!(convert_to_int32(&serde_json::json!(3)).ok(), Some(3));
        assert_eq!(convert_to_int32(&serde_json::json!(2.5)).ok(), Some(2));
        assert_eq!(convert_to_int32(&serde_json::json!(3.5)).ok(), Some(4));
        assert_eq!(convert_to_int32(&serde_json::json!(" 7 ")).ok(), Some(7));
        assert!(convert_to_int32(&serde_json::json!("x")).is_err());
        assert!(convert_to_int32(&serde_json::json!([1])).is_err());
    }
}
