//! `getSimilarSongs` and `getSimilarSongs2` (L2770): song radio from Last.fm's similar
//! tracks, the library's own copies preferred.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use octo_core::common::dotnet;
use octo_core::last_fm::{last_fm_radio_seed_normalizer, last_fm_radio_spacing};
use octo_core::models::domain::Song;
use octo_subsonic::SubsonicReply;
use octo_subsonic::subsonic_response_builder::{SUBSONIC_NAMESPACE, SUBSONIC_VERSION};
use octo_subsonic::xml::XElement;
use serde_json::{Map, Value};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use super::helpers_6a1::{SubsonicCall, int_param};
use crate::app::AppState;
use crate::services::last_fm::LastFmRadioTrackResolver;

pub async fn get_similar_songs(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "").to_string();
    let settings = state.settings.current();
    let count = int_param(&call.parameters, "count", 50)
        .clamp(1, settings.last_fm.effective_radio_track_count().max(1));

    // Subsonic spec: getSimilarSongs.view → key "similarSongs"; getSimilarSongs2.view →
    // "similarSongs2". Clients (Arpeggi) parse the v2 key strictly and ignore v1-shaped
    // responses when they called v2 — that's why the radio queue showed up empty.
    let path = call.path();
    let is_v2_request = dotnet::to_lower_invariant(&path).contains("getsimilarsongs2");
    let response_key = if is_v2_request {
        "similarSongs2"
    } else {
        "similarSongs"
    };

    if dotnet::is_blank(&id) {
        return builder
            .create_error(format, 10, "Missing id parameter")
            .into_response();
    }

    // Check if Last.fm radio is configured and enabled
    if !state.last_fm.is_radio_enabled() {
        debug!("Last.fm radio not configured, relaying to upstream server");
        // The path keeps its leading slash, so the upstream URL has a double slash after the
        // server's address, as the C# relayed it.
        return match call.proxy.relay(&path, &call.parameters).await {
            Ok(result) => call.file(&result.body, result.content_type.as_deref()),
            Err(_) => builder.create_response(format, response_key).into_response(),
        };
    }

    // Get the seed song metadata
    let mut artist_name = String::new();
    let mut track_title = String::new();

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);
    if is_external {
        // External song - get metadata from our service
        if let Some(song) = state
            .metadata_service
            .get_song(
                provider.as_deref().unwrap_or(""),
                external_id.as_deref().unwrap_or(""),
            )
            .await
        {
            artist_name = song.artist;
            track_title = song.title;
        }
    } else {
        // Local song - get metadata from Navidrome, with auth from the original request
        let mut get_song_params = call.parameters.clone();
        get_song_params.insert("id".into(), id.clone());
        get_song_params.insert("f".into(), "json".into());
        let read = async {
            let result = call
                .proxy
                .relay("rest/getSong", &get_song_params)
                .await
                .map_err(|e| e.to_string())?;
            let document: Value = serde_json::from_slice(&result.body).map_err(|e| e.to_string())?;
            let Some(song) = document
                .as_object()
                .and_then(|root| root.get("subsonic-response"))
                .and_then(|response| response.as_object().and_then(|r| r.get("song")))
            else {
                return Ok((String::new(), String::new()));
            };
            // GetString on a value that is not a string threw.
            let text = |name: &str| -> Result<String, String> {
                match song.as_object().and_then(|s| s.get(name)) {
                    None | Some(Value::Null) => Ok(String::new()),
                    Some(Value::String(s)) => Ok(s.clone()),
                    Some(_) if song.is_object() => Err(format!("{name} is not a string")),
                    Some(_) => Ok(String::new()),
                }
            };
            if !song.is_object() {
                return Err("the song is not an object".to_string());
            }
            Ok::<_, String>((text("artist")?, text("title")?))
        };
        match read.await {
            Ok((artist, title)) => {
                artist_name = artist;
                track_title = title;
            }
            Err(message) => {
                error!("Failed to get song metadata for {id}: {message}");
                return builder.create_response(format, response_key).into_response();
            }
        }
    }

    if artist_name.is_empty() || track_title.is_empty() {
        warn!("Could not get artist/title for song {id}");
        return builder.create_response(format, response_key).into_response();
    }

    // Strip collab/feature decoration so Last.fm finds the canonical artist.
    let lookup_artist = last_fm_radio_seed_normalizer::artist(&artist_name);
    let lookup_title = last_fm_radio_seed_normalizer::title(&track_title);
    info!(
        "Getting similar songs for {artist_name} - {track_title} (lookup: {lookup_artist} - {lookup_title})"
    );

    let similar_tracks = state
        .last_fm
        .get_similar_tracks(&lookup_artist, &lookup_title, count)
        .await;
    if similar_tracks.is_empty() {
        info!("No similar tracks found from Last.fm");
        return builder.create_response(format, response_key).into_response();
    }
    info!(
        "Found {} similar tracks from Last.fm; building radio queue",
        similar_tracks.len()
    );

    // For each Last.fm recommendation, prefer the local copy if we own it. Tracks the user
    // already has play at full FLAC quality from Navidrome and avoid the yt-dlp roundtrip
    // entirely. Lookups go in parallel against Navidrome under a cap of 10, which fits
    // comfortably inside Arpeggi's HTTP budget.
    let resolver = LastFmRadioTrackResolver::new(
        call.proxy.clone(),
        state.metadata_service.clone(),
        state.external_id_registry.clone(),
    );
    let gate = Semaphore::new(10);
    let resolving = similar_tracks.iter().take(count.max(0) as usize).map(|track| {
        let resolver = &resolver;
        let gate = &gate;
        let parameters = &call.parameters;
        async move {
            let _permit = gate.acquire().await.expect("the gate is never closed");
            resolver
                .resolve(&track.artist, &track.title, track.duration, parameters)
                .await
        }
    });
    let resolved: Vec<Song> = futures::future::join_all(resolving)
        .await
        .into_iter()
        .flatten()
        .collect();
    let resolved_songs = last_fm_radio_spacing::spread(
        &resolved,
        |song: &Song| Some(song.artist.clone()),
        Some(&artist_name),
    );

    let local_count = resolved_songs.iter().filter(|song| song.is_local).count();
    info!(
        "Radio for '{artist_name} - {track_title}' -> {} songs ({local_count} local, {} external)",
        resolved_songs.len(),
        resolved_songs.len() - local_count
    );

    // Track this radio queue so scrobble events can drive the sliding-window
    // prewarm of upcoming externals.
    state
        .radio_queues
        .register(resolved_songs.iter().map(|song| song.id.clone()));

    // Fire-and-forget prewarm for the top of the queue so the first few taps don't pay the
    // full cold yt-dlp resolve. Local songs are skipped by the prewarmer (they have no
    // registry entry).
    let metadata = Arc::clone(&state.metadata_service);
    let prewarm = resolved_songs.clone();
    tokio::spawn(async move { metadata.prewarm_you_tube_ids(&prewarm, 8).await });

    build_similar_songs_response(&state, format, &resolved_songs, response_key)
}

fn build_similar_songs_response(
    state: &AppState,
    format: &str,
    songs: &[Song],
    response_key: &str,
) -> Response {
    let builder = &state.subsonic_response_builder;
    if format == "json" {
        let json_songs: Vec<Value> = songs
            .iter()
            .map(|song| Value::Object(builder.convert_song_to_json(song)))
            .collect();
        let mut similar = Map::new();
        similar.insert("song".into(), Value::Array(json_songs));
        let mut body = Map::new();
        body.insert("status".into(), "ok".into());
        body.insert("version".into(), SUBSONIC_VERSION.into());
        body.insert(response_key.into(), Value::Object(similar));
        return builder.create_json_response(Value::Object(body)).into_response();
    }
    let mut similar = XElement::ns(SUBSONIC_NAMESPACE, response_key);
    for song in songs {
        similar.push(builder.convert_song_to_xml(song, Some(SUBSONIC_NAMESPACE)));
    }
    let document = XElement::ns(SUBSONIC_NAMESPACE, "subsonic-response")
        .attr("status", "ok")
        .attr("version", SUBSONIC_VERSION)
        .child(similar);
    SubsonicReply::xml(&document).into_response()
}
