//! `getPlaylists` (L357), `getPlaylist` (L426), and the playlist and internet radio
//! mutations (L688, L710), which refuse Octo's own read-only ids.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use octo_core::json::dom::Node;
use octo_subsonic::SubsonicReply;
use octo_subsonic::xml::XElement;
use serde_json::Value;
use tracing::warn;

use super::helpers_6a1::{
    RequestAborted, SubsonicCall, bootstrap_radio_profile, is_octo_playlist_id,
    is_successful_subsonic_response, materialize_station, playlist_names, playlist_stations,
    queue_refresh_if_stale, xml_value,
};
use crate::app::AppState;
use crate::services::library::LibraryActionPlaylistProvisioner;
use crate::services::subsonic::SubsonicResponseBuilderExt;

pub async fn get_playlists(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.clone();
    let builder = &state.subsonic_response_builder;
    let relay = call.proxy.relay_safe("rest/getPlaylists", &call.parameters).await;
    let relay = match relay {
        Some(relay) if !relay.body.is_empty() && is_successful_subsonic_response(&relay.body, &format) => {
            relay
        }
        Some(relay) if !relay.body.is_empty() => {
            return call.file(&relay.body, relay.content_type.as_deref());
        }
        _ => {
            return builder
                .create_error(&format, 0, "Unable to authenticate with Navidrome")
                .into_response();
        }
    };

    let username = call.param_or("u", "").to_string();

    // Navidrome answered ok above, so `u` is authenticated. Ensure this user's action
    // playlists exist, using the body we already have so the common case costs no extra
    // request. Fire-and-forget: creating a playlist must never delay the listing.
    let names = playlist_names(Some(&relay.body), &format);
    let provisioner = LibraryActionPlaylistProvisioner::new(call.proxy.clone(), state.settings.clone());
    let ensured_user = username.clone();
    let ensured_parameters = call.parameters.clone();
    tokio::spawn(async move {
        provisioner
            .ensure(&ensured_user, &names, &ensured_parameters)
            .await;
    });

    bootstrap_radio_profile(&state, &call.proxy, &username, &call.parameters).await;
    let stations = playlist_stations(&state, &username);
    queue_refresh_if_stale(&state, &username);
    let mix_settings = state.settings.current().generated_playlists.clone();
    let generated = if mix_settings.enabled {
        state.generated_playlists.list(&username, &call.parameters).await
    } else {
        Vec::new()
    };
    if stations.is_empty() && generated.is_empty() {
        return call.file(&relay.body, relay.content_type.as_deref());
    }

    let rows: Vec<serde_json::Map<String, Value>> = stations
        .iter()
        .map(|station| builder.radio_playlist_fields(station))
        .chain(
            generated
                .iter()
                .map(|mix| builder.generated_playlist_fields(mix, &mix_settings)),
        )
        .collect();
    let merged = if format.eq_ignore_ascii_case("json") {
        merge_json(
            &relay.body,
            "playlists",
            "playlist",
            rows.into_iter().map(Value::Object),
        )
        .map(|body| SubsonicReply::file(body, "application/json"))
    } else {
        merge_xml(
            &relay.body,
            "playlists",
            "playlist",
            rows.iter().map(field_attributes),
        )
        .map(|body| SubsonicReply::file(body, "application/xml"))
    };
    match merged {
        Some(reply) => reply.into_response(),
        None => {
            warn!("Could not merge Radio stations and mixes into getPlaylists");
            call.file(&relay.body, relay.content_type.as_deref())
        }
    }
}

/// A row's fields as XML attributes (`XmlValue`).
fn field_attributes(fields: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(name, value)| (name.clone(), xml_value(value)))
        .collect()
}

/// The JSON merge: `subsonic-response.{container}.{row}` made an array if it was not one, and
/// the rows appended. None when the body is not the object it should be (C# threw).
pub(super) fn merge_json(
    body: &[u8],
    container: &str,
    row: &str,
    rows: impl IntoIterator<Item = Value>,
) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut root = Node::parse(text).ok()?;
    let response = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?;
    if !matches!(response.get(container), Some(Node::Object(_))) {
        response.insert(container.to_string(), Node::object());
    }
    let holder = response.get_mut(container)?.as_object_mut()?;
    if !matches!(holder.get(row), Some(Node::Array(_))) {
        holder.insert(row.to_string(), Node::Array(Vec::new()));
    }
    let Some(Node::Array(list)) = holder.get_mut(row) else {
        return None;
    };
    list.extend(rows.into_iter().map(|value| to_node(&value)));
    Some(root.to_json_string(false).into_bytes())
}

/// The XML merge: the root's first `{container}` child (by local name), added if missing, and
/// one `{row}` element per attribute list in the root's namespace.
pub(super) fn merge_xml(
    body: &[u8],
    container: &str,
    row: &str,
    rows: impl IntoIterator<Item = Vec<(String, String)>>,
) -> Option<Vec<u8>> {
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let ns = root.namespace.clone();
    if !root.elements().any(|element| element.name == container) {
        root.push(XElement::in_namespace(ns.as_deref(), container));
    }
    let holder = root.elements_mut().find(|element| element.name == container)?;
    for attributes in rows {
        let mut element = XElement::in_namespace(ns.as_deref(), row);
        for (name, value) in attributes {
            element.set_attr(name, value);
        }
        holder.push(element);
    }
    Some(root.to_xml_string().into_bytes())
}

/// A serialised value as `JsonSerializer.SerializeToNode` made it: doubles in .NET's shortest
/// form.
pub(super) fn to_node(value: &Value) -> Node {
    match value {
        Value::Null => Node::Null,
        Value::Bool(b) => Node::Bool(*b),
        Value::Number(n) if n.is_i64() || n.is_u64() => Node::Number(n.to_string()),
        Value::Number(n) => Node::Number(octo_core::json::format_double(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Node::String(s.clone()),
        Value::Array(items) => Node::Array(items.iter().map(to_node).collect()),
        Value::Object(map) => Node::Object(map.iter().map(|(k, v)| (k.clone(), to_node(v))).collect()),
    }
}

pub async fn get_playlist(State(state): State<AppState>, req: Request) -> Response {
    let aborted = RequestAborted::new();
    let response = get_playlist_inner(&state, req, &aborted).await;
    aborted.answered();
    response
}

async fn get_playlist_inner(state: &AppState, req: Request, aborted: &RequestAborted) -> Response {
    let call = match SubsonicCall::read(state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "").to_string();
    let username = call.param_or("u", "").to_string();

    // A mix is the listener's own library, served as a playlist Navidrome has never heard of.
    // The ping is the auth check, as for a station: nothing about a user is revealed first.
    if let Some(mix) = state.generated_playlists.find(&username, &id) {
        let mut auth_only = call.parameters.clone();
        auth_only.shift_remove("id");
        let check = call.proxy.relay_safe("rest/ping", &auth_only).await;
        if !check.is_some_and(|check| is_successful_subsonic_response(&check.body, format)) {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        // The C# let a failure here escape to the global handler.
        let entries = match state
            .generated_playlists
            .materialize(&username, &mix, &call.parameters, &aborted.token)
            .await
        {
            Ok(entries) => entries,
            Err(error) => return crate::http::error::AppError::Internal(error).into_response(),
        };
        let settings = state.settings.current().generated_playlists.clone();
        return builder
            .create_generated_playlist_response(format, &mix, &settings, &entries)
            .into_response();
    }

    let Some(station) = playlist_stations(state, &username)
        .into_iter()
        .find(|item| item.id == id)
    else {
        return match call.proxy.relay_safe("rest/getPlaylist", &call.parameters).await {
            Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
            None => builder
                .create_error(format, 0, "Playlist not found")
                .into_response(),
        };
    };
    let mut auth = call.parameters.clone();
    auth.shift_remove("id");
    let ping = call.proxy.relay_safe("rest/ping", &auth).await;
    if !ping.is_some_and(|ping| is_successful_subsonic_response(&ping.body, format)) {
        return builder
            .create_error(format, 40, "Wrong username or password")
            .into_response();
    }

    let mut songs = materialize_station(state, &call.proxy, &station, &call.parameters).await;
    songs = state
        .generated_playlists
        .blend_into_discovery(&username, &station, songs, &call.parameters)
        .await;

    // Before the catalog swap below: these rows are this response's own, while a catalog
    // row is shared with every other response that serves it.
    state.metadata_service.complete_song_lengths(&mut songs);

    // A song this user's sync catalog also holds goes out as the catalog describes it:
    // same album, and filed under the library's own artist where there is one. A syncing
    // client stores whichever description it read last, so the two must not disagree.
    let songs: Vec<_> = songs
        .into_iter()
        .map(|song| {
            if song.is_local {
                return song;
            }
            state
                .sync_catalog
                .try_get_song(&username, &song.id)
                .unwrap_or(song)
        })
        .collect();
    state
        .radio_queues
        .register(songs.iter().map(|song| song.id.clone()));
    let metadata = Arc::clone(&state.metadata_service);
    let prewarm = songs.clone();
    tokio::spawn(async move { metadata.prewarm_you_tube_ids(&prewarm, 8).await });
    queue_refresh_if_stale(state, &username);
    builder
        .create_radio_playlist_response(format, &station, &songs)
        .into_response()
}

pub async fn mutate_internet_radio_station(State(state): State<AppState>, req: Request) -> Response {
    mutate(
        state,
        req,
        "id",
        "updateInternetRadioStation",
        "Unable to update internet radio station",
    )
    .await
}

pub async fn mutate_playlist(State(state): State<AppState>, req: Request) -> Response {
    mutate(
        state,
        req,
        "playlistId",
        "updatePlaylist",
        "Unable to update playlist",
    )
    .await
}

/// Refuses Octo's own ids, and relays everything else as `rest/<last path segment>` in the
/// client's own spelling.
async fn mutate(state: AppState, req: Request, id_key: &str, _fallback: &str, failure: &str) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    // updatePlaylist names its playlist `playlistId`, falling back to `id`.
    let id = call
        .param(id_key)
        .or_else(|| call.param("id"))
        .unwrap_or("")
        .to_string();
    if is_octo_playlist_id(&id) {
        return state
            .subsonic_response_builder
            .create_error(format, 70, "Octo's generated playlists are read-only")
            .into_response();
    }
    // `Request.Path.Value?.Split('/').LastOrDefault()?.Replace(".view", "")`: the path always
    // has a last segment, so the C#'s fallback name was never used.
    let path = call.path();
    let endpoint = path.split('/').next_back().unwrap_or("").replace(".view", "");
    match call
        .proxy
        .relay_safe(&format!("rest/{endpoint}"), &call.parameters)
        .await
    {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => state
            .subsonic_response_builder
            .create_error(format, 0, failure)
            .into_response(),
    }
}
