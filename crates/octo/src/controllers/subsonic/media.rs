//! `stream`, `getCoverArt` and `getTranscodeDecision` (`SubsonicController.Stream` L1451,
//! `GetCoverArt` L2012, `GetTranscodeDecision` L3156; endpoints.md §3.4).

use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::FutureExt;
use octo_core::common::dotnet::{self, is_blank};
use octo_core::common::{SongIdentity, playlist_id_helper};
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_media::cover::{CoverSeed, ListCover, list_kinds};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::helpers_6a2::{
    SubsonicCall, file, is_first_byte_request, is_successful_subsonic_response, refuse_unless_signed_in,
    requester_for, signed_in_user,
};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult, json_status, problem};
use crate::http::routes::RouteSet;
use crate::http::static_files::{RangeOutcome, parse_range};
use crate::services::common::track_acquisition_queue::Completion;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::GeneratedPlaylist;
use crate::services::subsonic::subsonic_proxy_service::content_type_of;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("stream", get(stream).post(stream))
        .subsonic("getCoverArt", get(get_cover_art).post(get_cover_art))
        .subsonic(
            "getTranscodeDecision",
            get(get_transcode_decision).post(get_transcode_decision),
        )
}

// ---------------------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------------------

/// Downloads on-the-fly if needed, or streams directly in Stream mode.
pub async fn stream(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();
    // Needed so a failure here can answer with a real Subsonic error envelope instead
    // of the bare JSON this method used to emit.
    let format = call.format();

    if is_blank(&id) {
        return Ok(json_status(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "Missing id parameter" }),
        ));
    }

    let (is_external, provider, external_id) = state.local_library.parse_song_id(&id);

    // Verbose entry log: every stream call gets a single line tagged with
    // the client + id + isExternal + Range + UA + key headers. Diagnostics
    // for "client X never plays external songs" — if a tap doesn't even
    // reach this log line, the client is filtering on its side.
    let client_name = call.param_opt("c").unwrap_or("?").to_string();
    let range_in = call.header("Range").unwrap_or_else(|| "(none)".into());
    let ua_in = call.header("User-Agent").unwrap_or_else(|| "(none)".into());
    info!(
        "STREAM-IN client={client_name} id={id} isExternal={} range={range_in} ua={ua_in}",
        if is_external { "True" } else { "False" }
    );

    if !is_external {
        return Ok(call.proxy.relay_stream(&call.parameters).await);
    }

    // Navidrome checks the sign-in on everything relayed to it, but it never sees an outside
    // song, so without this anyone who can reach Octo could play through it with no account.
    if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
        return Ok(refused);
    }

    let provider = provider.unwrap_or_default();
    let external_id = external_id.unwrap_or_default();

    // A local file may only be served under an external id when this session DECLARES
    // that id as lossless. search3 already told the client a suffix, bitrate and size,
    // and a player picks its decoder from those, so handing back different bytes is
    // what makes tracks silently refuse to start. With the default settings the
    // lossless copy is reached as its own library track after the rescan instead.
    if state.settings.current().subsonic.wait_for_lossless_on_play {
        let local_path = state
            .local_library
            .get_local_path_for_external_song(&provider, &external_id)
            .await;
        if let Some(local_path) = local_path
            && Path::new(&local_path).is_file()
        {
            return Ok(serve_file(&call, &local_path).await?);
        }
    }

    match play_external(&state, &call, &provider, &external_id, &id, &format).await {
        Ok(response) => Ok(response),
        Err(error) => {
            error!("Failed to stream track {id}: {error:#}");
            Ok(json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": format!("Failed to stream: {error}") }),
            ))
        }
    }
}

/// The body of the C#'s try: queue the play, wait for a lossless copy when asked to, else
/// the preview. An error here is the 500 `{"error":"Failed to stream: ..."}`. (A client that
/// hangs up drops this future, which is what the C#'s `OperationCanceledException` catch
/// answered with an empty result.)
async fn play_external(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    id: &str,
    format: &str,
) -> anyhow::Result<Response> {
    let settings = state.settings.current().subsonic.clone();
    // Lossless-on-play remains an explicit opt-in. Normal playback starts no
    // acquisition unless DownloadOnPlay or LidarrAlbumOnPlay are on: owned ids
    // already went to Navidrome above, and missing ids stream from YouTube below.
    // Hearts are the normal permanent-copy gesture.
    // Only a request from the first byte is a play. Clients ask again with a later
    // Range on every seek and while buffering. A transcoded request (format,
    // maxBitRate) is still a play and counts.
    let range = call.header("Range");
    if is_first_byte_request(call.method().as_str(), range.as_deref()) {
        let who = signed_in_user(state, call).await.map_err(anyhow::Error::new)?;
        state.heart_acquisition_coordinator.queue_play(
            provider,
            external_id,
            requester_for(state, Some(&who)).as_deref(),
            Some(id),
            Some(&who),
        );
    }
    if settings.wait_for_lossless_on_play {
        let who = signed_in_user(state, call).await.map_err(anyhow::Error::new)?;
        let acquisition = state.track_acquisition_queue.enqueue(
            provider,
            external_id,
            false,
            false,
            true,
            None,
            true,
            requester_for(state, Some(&who)).as_deref(),
            false,
            None,
        );
        return serve_acquired(state, call, acquisition, provider, external_id, id, format, true).await;
    }

    if let Some(direct) = try_direct_stream(state, call, provider, external_id, id).await? {
        return Ok(direct);
    }

    warn!("Direct stream not available for {id}");
    Ok(state
        .subsonic_response_builder
        .create_error(format, 70, "No playable source found for this track")
        .into_response())
}

/// Wait for a queued acquisition and serve the file.
///
/// Abandoning the WAIT leaves the transfer running, so a client that gives up costs it
/// nothing.
#[allow(clippy::too_many_arguments)]
async fn serve_acquired(
    state: &AppState,
    call: &SubsonicCall,
    acquisition: Completion,
    provider: &str,
    external_id: &str,
    id: &str,
    format: &str,
    allow_preview_fallback: bool,
) -> anyhow::Result<Response> {
    // Above 0, the wait is bounded and the preview stands in while the fetch keeps
    // running in the background; the next play of this id serves the landed file.
    let timeout = state.settings.current().subsonic.lossless_wait_timeout_seconds.max(0);
    let fallback = allow_preview_fallback && timeout > 0;

    let outcome = if fallback {
        match tokio::time::timeout(Duration::from_secs(timeout as u64), acquisition.wait()).await {
            Ok(outcome) => outcome.map_err(|e| (e.to_string(), false)),
            Err(_) => Err(("The operation has timed out.".to_string(), true)),
        }
    } else {
        acquisition.wait().await.map_err(|e| (e.to_string(), false))
    };

    let path = match outcome {
        Ok(path) => path,
        Err((message, timed_out)) => {
            if fallback {
                // The user opted into a bounded wait, trading the declared-lossless
                // contract for playback that starts. Strict clients may refuse the
                // lossy bytes; timeout 0 keeps the contract exact.
                let reason = if timed_out {
                    format!("timeout {timeout}s")
                } else {
                    message.clone()
                };
                info!(
                    "Lossless wait ended early for {id} ({reason}); serving the preview while the fetch continues"
                );
                if let Some(preview) = try_direct_stream(state, call, provider, external_id, id).await? {
                    return Ok(preview);
                }
            } else {
                // Never fall back to the lossy stream here. This session declared the id
                // lossless, so lossy bytes would be the same contract violation in reverse.
                warn!("Lossless acquisition failed for {id}: {message}");
            }
            return Ok(state
                .subsonic_response_builder
                .create_error(format, 70, &format!("Could not fetch a lossless copy: {message}"))
                .into_response());
        }
    };

    if !Path::new(&path).is_file() {
        return Ok(state
            .subsonic_response_builder
            .create_error(format, 70, "Lossless copy is no longer on disk")
            .into_response());
    }
    serve_file(call, &path).await.map_err(anyhow::Error::new)
}

/// Proxy the lossy preview straight from the CDN. `None` when no source resolved.
async fn try_direct_stream(
    state: &AppState,
    call: &SubsonicCall,
    provider: &str,
    external_id: &str,
    id: &str,
) -> anyhow::Result<Option<Response>> {
    // Forward the client's Range header up the chain so the shim can
    // ask googlevideo for the requested byte range and we can return
    // a proper 206. iOS Subsonic clients refuse to play non-FLAC
    // audio without working byte-range support — our prior 200/none
    // response was what was making Arpeggi/Narjo silently drop
    // every external song from the queue.
    let range_header = call.header("Range");

    let Some(direct) = state
        .download_service
        .get_direct_stream(
            provider,
            external_id,
            range_header.as_deref(),
            &CancellationToken::new(),
        )
        .await?
    else {
        return Ok(None);
    };

    info!(
        "Direct streaming track {id} ({}, status={})",
        direct.quality.as_deref().unwrap_or(""),
        direct.status_code
    );

    // Manual stream copy: the network stream isn't seekable, so the upstream's status code +
    // Content-Range are forwarded verbatim and the bytes copied to the response body.
    let mut response = Response::new(Body::from_stream(direct.audio_stream));
    *response.status_mut() = StatusCode::from_u16(direct.status_code).unwrap_or(StatusCode::OK);
    let headers = response.headers_mut();
    if let Some(length) = direct.content_length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    if let Ok(content_type) = HeaderValue::from_str(&direct.content_type) {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(range) = direct.content_range.filter(|r| !r.is_empty())
        && let Ok(range) = HeaderValue::from_str(&range)
    {
        headers.insert(header::CONTENT_RANGE, range);
    }
    Ok(Some(response))
}

/// `File(File.OpenRead(path), GetContentType(path), enableRangeProcessing: true)`: the file
/// with ASP.NET's single-range support, and no ETag or Last-Modified.
async fn serve_file(call: &SubsonicCall, path: &str) -> Result<Response, AppError> {
    let content_type = content_type_for_file(path);
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();

    // An If-Range the file cannot be checked against (it has no validators) sends it whole.
    let range = call
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| !call.headers().contains_key(header::IF_RANGE))
        .map(|v| parse_range(v, length))
        .unwrap_or(RangeOutcome::Whole);

    let (status, from, count, content_range) = match range {
        RangeOutcome::Whole => (StatusCode::OK, 0, length, None),
        RangeOutcome::Partial(from, to) => (
            StatusCode::PARTIAL_CONTENT,
            from,
            to - from + 1,
            Some(format!("bytes {from}-{to}/{length}")),
        ),
        RangeOutcome::Unsatisfiable => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            let headers = response.headers_mut();
            if let Ok(range) = HeaderValue::from_str(&format!("bytes */{length}")) {
                headers.insert(header::CONTENT_RANGE, range);
            }
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            return Ok(response);
        }
    };

    if from > 0 {
        file.seek(SeekFrom::Start(from)).await?;
    }
    let body = Body::from_stream(ReaderStream::new(file.take(count)));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(range) = content_range.and_then(|r| HeaderValue::from_str(&r).ok()) {
        headers.insert(header::CONTENT_RANGE, range);
    }
    Ok(response)
}

/// `GetContentType`: the audio type of a file by its extension, `audio/mpeg` by default.
fn content_type_for_file(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .map(|e| dotnet::to_lower_invariant(&e.to_string_lossy()))
        .unwrap_or_default();
    match extension.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        _ => "audio/mpeg",
    }
}

// ---------------------------------------------------------------------------------------
// getCoverArt
// ---------------------------------------------------------------------------------------

/// Proxies external covers. Uses type from ID to determine which API to call.
/// Format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259, ext-deezer-album-96126)
pub async fn get_cover_art(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let id = call.param("id").to_string();

    if is_blank(&id) {
        return Ok(problem(StatusCode::NOT_FOUND));
    }

    // Stable, cached Octo-branded station artwork. Deliberately static: playlist
    // requests never perform a live cover mosaic build.
    if id.eq_ignore_ascii_case("octo-radio") {
        return Ok(serve_placeholder(&state, true).await);
    }

    // Generated radio IDs already resolve through the per-user state store,
    // so station artwork follows the current station name without creating
    // a parallel metadata record or exposing that name in the cover ID.
    if let Some(station) = state.last_fm_radio_state.find_station(call.param("u"), &id) {
        let seeds = station_cover_seeds(&state, &station);
        let list = ListCover {
            name: station.name.clone(),
            label: station_cover_label(&station),
            kind: list_kinds::RADIO.to_string(),
            seeds: Some(Arc::new(move || {
                let seeds = seeds.clone();
                async move { Ok(seeds) }.boxed()
            })),
            song_count: Some(i32::try_from(station.tracks.len()).unwrap_or(i32::MAX)),
        };
        let bytes = state
            .cover_art_service
            .get_list_cover(&list, requested_cover_size(&call))
            .await;
        return Ok(file(bytes, "image/jpeg"));
    }

    // A mix is the listener's own library, so its cover carries no Octo mark: the logo says
    // where a result came from, and this came from them.
    let cover_user = call.param("u").to_string();
    if let Some(mix) = state.generated_playlists.find(&cover_user, &id) {
        let song_count = state
            .generated_playlists
            .drawn(&cover_user, &mix)
            .map(|songs| i32::try_from(songs.len()).unwrap_or(i32::MAX));
        let seeds_state = state.clone();
        let seeds_proxy = call.proxy.clone();
        let parameters = call.parameters.clone();
        let seeds_mix = mix.clone();
        let list = ListCover {
            name: mix.name.clone(),
            label: Some(mix.label.clone()),
            kind: list_kinds::MIX.to_string(),
            seeds: Some(Arc::new(move || {
                let state = seeds_state.clone();
                let proxy = seeds_proxy.clone();
                let parameters = parameters.clone();
                let mix = seeds_mix.clone();
                let user = cover_user.clone();
                async move { Ok(mix_cover_seeds(&state, &proxy, &user, &mix, &parameters).await) }.boxed()
            })),
            song_count,
        };
        let bytes = state
            .cover_art_service
            .get_list_cover(&list, requested_cover_size(&call))
            .await;
        return Ok(file(bytes, "image/jpeg"));
    }

    // Playlist covers haven't changed — keep the existing path.
    if playlist_id_helper::is_external_playlist(Some(&id)) {
        return Ok(match external_playlist_cover(&state, &id).await {
            Ok(Some(response)) => response,
            Ok(None) => serve_placeholder(&state, true).await,
            Err(e) => {
                error!("Error getting playlist cover art for {id}: {e:#}");
                serve_placeholder(&state, true).await
            }
        });
    }

    // Registry-backed id (song / album / artist). Resolve to artist+title via the
    // registry and look the cover up on iTunes. Watermark with the Octo logo so
    // radio-sourced art is visually distinct from local-library art.
    // The Octo app marks songs outside the library itself, so its covers come back
    // plain, and a missing one is a 404 it draws its own empty tile for. Every other
    // client keeps the badge, the only sign it gets that a song came from Octo's search.
    let plain = draws_its_own_marks(&call);
    if let Some(routing) = state.external_id_registry.lookup(&id) {
        let routing = routing.snapshot();
        let raw = state.cover_art_aggregator.get_cover(&routing, false).await;
        let Some(raw) = raw else {
            debug!(
                "cover art all-source miss for {:?} '{} - {}/{}', serving placeholder",
                routing.kind,
                routing.artist.as_deref().unwrap_or(""),
                routing.title.as_deref().unwrap_or(""),
                routing.album.as_deref().unwrap_or("")
            );
            return Ok(missing_cover(&state, plain).await);
        };
        let watermarked = if plain { raw.to_vec() } else { badge(&state, raw.to_vec()).await };
        return Ok(file(watermarked, "image/jpeg"));
    }

    // Legacy "ext-album-{hash}" / "ext-artist-{hash}" ids that pre-date the
    // registry. We can't reverse-resolve them, but returning a 404 makes
    // Arpeggio drop the song, so serve the Octo placeholder instead.
    if dotnet::starts_with_ignore_case(&id, "ext-album-") || dotnet::starts_with_ignore_case(&id, "ext-artist-") {
        return Ok(missing_cover(&state, plain).await);
    }

    // Existing ext-{provider}-{type}-{id} path (Deezer/Tidal-era, kept for
    // compatibility with any in-flight clients).
    let (is_external, cover_provider, kind, cover_external_id) = state.local_library.parse_external_id(&id);
    if is_external {
        let provider = cover_provider.unwrap_or_default();
        let external_id = cover_external_id.unwrap_or_default();
        let metadata = &state.music_metadata;
        let cover_url = match kind.as_deref() {
            Some("artist") => metadata
                .get_artist(&provider, &external_id)
                .await
                .and_then(|a| a.image_url),
            Some("album") => metadata
                .get_album(&provider, &external_id)
                .await
                .and_then(|a| a.cover_art_url),
            _ => match metadata
                .get_song(&provider, &external_id)
                .await
                .and_then(|s| s.cover_art_url)
            {
                Some(url) => Some(url),
                None => metadata
                    .get_album(&provider, &external_id)
                    .await
                    .and_then(|a| a.cover_art_url),
            },
        };

        if let Some(cover_url) = cover_url {
            // Not caught: a failed fetch reached the global handler.
            let response = state.http.get(&cover_url).send().await?;
            if response.status().is_success() {
                let image_bytes = response.bytes().await?.to_vec();
                let watermarked = if plain { image_bytes } else { badge(&state, image_bytes).await };
                return Ok(file(watermarked, "image/jpeg"));
            }
        }
        return Ok(missing_cover(&state, plain).await);
    }

    // Local library — proxy to Navidrome unchanged.
    match call.proxy.relay("rest/getCoverArt", &call.parameters).await {
        Ok(result) => Ok(file(
            result.body,
            result.content_type.as_deref().unwrap_or("image/jpeg"),
        )),
        Err(e) => {
            // Unbranded on purpose. This is the user's own file; stamping the Octo
            // logo on it makes Octo look like it is claiming a track the user
            // already owned. Reading embedded art off a cloud-backed mount can take
            // seconds cold, so this path is reached by ordinary slowness, not just
            // by missing art — all the more reason not to brand it.
            debug!("cover art relay failed for local id {id}: {e}");
            Ok(serve_placeholder(&state, false).await)
        }
    }
}

/// A `pl-{provider}-{id}` playlist's own picture, `None` for the placeholder.
async fn external_playlist_cover(state: &AppState, id: &str) -> anyhow::Result<Option<Response>> {
    let (provider, external_id) = playlist_id_helper::parse_playlist_id(id)?;
    let Some(playlist) = state.music_metadata.get_playlist(&provider, &external_id).await else {
        return Ok(None);
    };
    let Some(cover_url) = playlist.cover_url.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let image = state.http.get(&cover_url).send().await?;
    if !image.status().is_success() {
        return Ok(None);
    }
    let content_type = content_type_of(image.headers()).unwrap_or_else(|| "image/jpeg".to_string());
    let bytes = image.bytes().await?;
    Ok(Some(file(bytes.to_vec(), &content_type)))
}

/// What a cover that could not be found answers: a 404 for the Octo app, which draws its own
/// tile, and the branded placeholder for everyone else.
async fn missing_cover(state: &AppState, plain: bool) -> Response {
    if plain {
        problem(StatusCode::NOT_FOUND)
    } else {
        serve_placeholder(state, true).await
    }
}

/// `_coverArtService.AddOctoBadge(raw)`, off the async threads.
async fn badge(state: &AppState, raw: Vec<u8>) -> Vec<u8> {
    let service = Arc::clone(&state.cover_art_service);
    let fallback = raw.clone();
    tokio::task::spawn_blocking(move || service.add_octo_badge(&raw))
        .await
        .unwrap_or(fallback)
}

/// The size a client asked a cover at, if it asked.
fn requested_cover_size(call: &SubsonicCall) -> Option<i32> {
    call.param("size").parse::<i32>().ok().filter(|size| *size > 0)
}

/// A genre or pinned station's tag, which stands in for its colour when its songs give none.
fn station_cover_label(station: &LastFmRadioStation) -> Option<String> {
    match station.kind {
        LastFmRadioStationKind::Genre | LastFmRadioStationKind::Pinned | LastFmRadioStationKind::Discovery => {
            station.seeds.first().cloned()
        }
        _ => None,
    }
}

/// The songs whose covers colour a station's cover: its seed artists' songs first (the
/// artist of an artist station, the top seeds of Your Mix), then its first songs, one per
/// album, four at most. Looked up like any song's cover outside the library.
pub(crate) fn station_cover_seeds(state: &AppState, station: &LastFmRadioStation) -> Vec<CoverSeed> {
    let seed_artists: Vec<String> = match station.kind {
        LastFmRadioStationKind::Artist | LastFmRadioStationKind::YourMix | LastFmRadioStationKind::Starter => station
            .seeds
            .iter()
            .take(3)
            .map(|seed| SongIdentity::key(seed))
            .filter(|key| !key.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let rank = |artist: &str| {
        let key = SongIdentity::key(artist);
        seed_artists
            .iter()
            .position(|seed| *seed == key)
            .unwrap_or(seed_artists.len())
    };
    let mut ordered: Vec<(usize, usize, &_)> = station
        .tracks
        .iter()
        .filter(|track| !is_blank(&track.artist) && !is_blank(&track.title))
        .enumerate()
        .map(|(order, track)| (rank(&track.artist), order, track))
        .collect();
    ordered.sort_by_key(|(rank, order, _)| (*rank, *order));

    let mut seen = std::collections::HashSet::new();
    let mut seeds = Vec::new();
    for (_, _, track) in ordered {
        let album_or_title = track.album.as_deref().unwrap_or(&track.title);
        let distinct = format!(
            "{}|{}",
            SongIdentity::key(&track.artist),
            SongIdentity::key(album_or_title)
        );
        if !seen.insert(distinct) {
            continue;
        }
        let routing = SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(track.artist.clone()),
            title: Some(track.title.clone()),
            album: track.album.clone(),
            ..Default::default()
        };
        let covers = Arc::clone(&state.cover_art_aggregator);
        seeds.push(CoverSeed {
            identity: format!(
                "song|{}|{}",
                SongIdentity::key(&track.artist),
                SongIdentity::key(&track.title)
            ),
            fetch: Arc::new(move || {
                let covers = Arc::clone(&covers);
                let routing = routing.clone();
                async move { Ok(covers.get_cover(&routing, false).await.map(|b| b.to_vec())) }.boxed()
            }),
        });
        if seeds.len() == 4 {
            break;
        }
    }
    seeds
}

/// The songs whose covers colour a mix's cover: the first of this period's draw, one per
/// album cover, four at most, read from Navidrome as the listener. A period's first draw is
/// made only for a caller Navidrome accepts, as opening the mix would.
async fn mix_cover_seeds(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    username: &str,
    mix: &GeneratedPlaylist,
    parameters: &octo_subsonic::Parameters,
) -> Vec<CoverSeed> {
    let auth: octo_subsonic::Parameters = parameters
        .iter()
        .filter(|(key, _)| key.as_str() != "id" && key.as_str() != "size")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let songs = match state.generated_playlists.drawn(username, mix) {
        Some(songs) => songs,
        None => {
            let ping = proxy.relay_safe("rest/ping", &auth).await;
            let format = auth.get("f").map_or("xml", String::as_str);
            if !ping.is_some_and(|ping| is_successful_subsonic_response(&ping.body, format)) {
                return Vec::new();
            }
            // Not cancelled with the cover: a draw that finishes late still serves the next request.
            let service = Arc::clone(&state.generated_playlists);
            let (user, mix, parameters) = (username.to_string(), mix.clone(), parameters.clone());
            let draw = tokio::spawn(async move {
                service
                    .materialize(&user, &mix, &parameters, &CancellationToken::new())
                    .await
            });
            match draw.await {
                Ok(Ok(songs)) => songs,
                _ => return Vec::new(),
            }
        }
    };

    let mut covers: Vec<String> = Vec::new();
    for song in &songs {
        let cover = match song.get("coverArt") {
            None | Some(octo_core::json::dom::Node::Null) => continue,
            Some(octo_core::json::dom::Node::String(text)) => text.clone(),
            Some(other) => other.to_json_string(false),
        };
        if cover.is_empty() || covers.contains(&cover) {
            continue;
        }
        covers.push(cover);
        if covers.len() == 4 {
            break;
        }
    }
    covers
        .into_iter()
        .map(|cover| {
            let proxy = proxy.clone();
            let mut asking = auth.clone();
            asking.insert("id".into(), cover.clone());
            asking.insert("size".into(), "128".into());
            CoverSeed {
                identity: format!("navidrome|{cover}"),
                fetch: Arc::new(move || {
                    let proxy = proxy.clone();
                    let asking = asking.clone();
                    async move {
                        let picture = proxy.relay("rest/getCoverArt", &asking).await?;
                        let image = picture
                            .content_type
                            .as_deref()
                            .is_some_and(|ct| dotnet::starts_with_ignore_case(ct, "image/"));
                        Ok(image.then(|| picture.body.to_vec()))
                    }
                    .boxed()
                }),
            }
        })
        .collect()
}

/// Whether the request comes from the Octo app, which shows on its own artwork that a song
/// is not in the library, so it wants covers without the badge. Told apart by the Subsonic
/// client name it sends on every call.
pub(crate) fn draws_its_own_marks(call: &SubsonicCall) -> bool {
    call.param("c").trim().eq_ignore_ascii_case("Octo")
}

/// Returns a 200 response with the Octo placeholder JPEG. Used in every code path that
/// previously returned 404 — Subsonic clients (Arpeggio especially) drop play-queue entries
/// whose cover-art request fails, so we always serve something rather than fail.
pub(crate) async fn serve_placeholder(state: &AppState, branded: bool) -> Response {
    let service = Arc::clone(&state.cover_art_service);
    let bytes = tokio::task::spawn_blocking(move || service.get_placeholder_cover(branded))
        .await
        .unwrap_or_default();
    if bytes.is_empty() {
        return problem(StatusCode::NOT_FOUND);
    }
    file(bytes, "image/jpeg")
}

// ---------------------------------------------------------------------------------------
// getTranscodeDecision
// ---------------------------------------------------------------------------------------

/// OpenSubsonic transcoding extension. Feishin posts here before /rest/stream to ask the
/// server "should I transcode this or play it directly?" Navidrome implements this for local
/// songs. For external (Octo placeholder) songs the upstream relay returns nothing useful and
/// Feishin gets stuck — won't even issue the /rest/stream call. So we hijack: external IDs
/// always direct-play, local IDs pass through to Navidrome's real implementation.
pub async fn get_transcode_decision(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let media_id = call.param("mediaId").to_string();
    let (is_external, _, _) = state.local_library.parse_song_id(&media_id);

    if is_external {
        debug!("getTranscodeDecision: direct-play for external id {media_id}");
        return Ok(direct_play_response(&state));
    }

    match call
        .proxy
        .relay("rest/getTranscodeDecision.view", &call.parameters)
        .await
    {
        Ok(result) => Ok(file(
            result.body,
            result.content_type.as_deref().unwrap_or("application/json"),
        )),
        Err(crate::services::subsonic::RelayError::Http(message)) => {
            // Navidrome may be stock-Subsonic without the OpenSubsonic transcoding
            // extension. Returning a non-200 also makes Feishin fall back to the
            // direct stream URL, but a positive direct-play decision is cleaner.
            debug!("getTranscodeDecision local relay failed ({message}); returning direct-play");
            Ok(direct_play_response(&state))
        }
        Err(other) => Err(other.into()),
    }
}

/// canDirectPlay:true is the only field Feishin's controller checks on the happy path — see
/// Feishin's subsonic-controller.ts: requiresTranscoding = !td?.canDirectPlay. Returning the
/// minimal envelope lets it advance to /rest/stream which is where our own controller takes
/// over for externals.
fn direct_play_response(state: &AppState) -> Response {
    state
        .subsonic_response_builder
        .create_json_response(json!({
            "status": "ok",
            "version": "1.16.1",
            "transcodeDecision": { "canDirectPlay": true, "canTranscode": false },
        }))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_typed_by_its_extension() {
        for (path, content_type) in [
            ("/m/a.mp3", "audio/mpeg"),
            ("/m/a.FLAC", "audio/flac"),
            ("/m/a.ogg", "audio/ogg"),
            ("/m/a.m4a", "audio/mp4"),
            ("/m/a.wav", "audio/wav"),
            ("/m/a.aac", "audio/aac"),
            ("/m/a.opus", "audio/mpeg"),
            ("/m/a", "audio/mpeg"),
        ] {
            assert_eq!(content_type_for_file(path), content_type, "{path}");
        }
    }
}
