//! `getAlbumInfo2`/`getAlbumInfo` (L3624) and `getArtistInfo2`/`getArtistInfo` (L3655).

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};

use super::helpers_6a1::SubsonicCall;
use crate::app::AppState;

pub async fn get_album_info2(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "");
    let (is_external, provider, external_id) = state.local_library.parse_song_id(id);

    if is_external {
        let album = state
            .metadata_service
            .get_album(provider.as_deref().unwrap_or(""), external_id.as_deref().unwrap_or(""))
            .await;
        let url = album.and_then(|album| album.cover_art_url).unwrap_or_default();
        return builder
            .create_info_response(
                format,
                "albumInfo",
                &[
                    ("notes", ""),
                    ("smallImageUrl", &url),
                    ("mediumImageUrl", &url),
                    ("largeImageUrl", &url),
                ],
            )
            .into_response();
    }

    // Always the v2 endpoint upstream, even for a v1 request.
    match call.proxy.relay_safe("rest/getAlbumInfo2", &call.parameters).await {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => builder.create_response(format, "albumInfo").into_response(),
    }
}

pub async fn get_artist_info2(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "");
    let (is_external, provider, external_id) = state.local_library.parse_song_id(id);

    if is_external {
        let artist = state
            .metadata_service
            .get_artist(provider.as_deref().unwrap_or(""), external_id.as_deref().unwrap_or(""))
            .await;
        let url = artist.and_then(|artist| artist.image_url).unwrap_or_default();
        // The v1 request still gets the artistInfo2 element name.
        return builder
            .create_info_response(
                format,
                "artistInfo2",
                &[
                    ("biography", ""),
                    ("smallImageUrl", &url),
                    ("mediumImageUrl", &url),
                    ("largeImageUrl", &url),
                ],
            )
            .into_response();
    }

    match call.proxy.relay_safe("rest/getArtistInfo2", &call.parameters).await {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => builder.create_response(format, "artistInfo2").into_response(),
    }
}
