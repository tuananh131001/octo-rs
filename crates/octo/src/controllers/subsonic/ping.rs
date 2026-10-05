//! `ping` (L207) and `getRandomSongs` (L242).

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use octo_core::common::dotnet;
use octo_subsonic::SubsonicReply;

use super::helpers_6a1::SubsonicCall;
use crate::app::AppState;
use crate::http::error::AppError;
use crate::services::subsonic::subsonic_proxy_service::is_absolute_uri;

// -------------------------------------------------------------------------
// ping — the first call every Subsonic client makes. We make it the moment a
// broken setup explains itself, instead of relaying blindly and returning an
// opaque error when the Navidrome URL is missing or unreachable.
// -------------------------------------------------------------------------
pub async fn ping(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let url = state.settings.current().subsonic.url.clone().unwrap_or_default();

    if dotnet::is_blank(&url) || !is_absolute_uri(&url) {
        return state
            .subsonic_response_builder
            .create_error(
                &call.format,
                0,
                &format!(
                    "Octo isn't configured yet. Open {}://{}/admin and set your Navidrome URL \
                     (SUBSONIC_URL), then point this client at Octo instead of Navidrome.",
                    call.scheme(),
                    call.host()
                ),
            )
            .into_response();
    }

    // Relay to Navidrome so real credentials are validated there. A connection
    // failure means Octo can't reach the configured URL; pass a successful
    // (or auth-failed) Navidrome envelope straight through otherwise.
    match call.proxy.relay_safe("rest/ping.view", &call.parameters).await {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => state
            .subsonic_response_builder
            .create_error(
                &call.format,
                0,
                &format!(
                    "Octo can't reach Navidrome at {url}. Check the URL is correct and reachable from \
                     the Octo container (use a LAN IP or service name, not localhost)."
                ),
            )
            .into_response(),
    }
}

// ---------------------------------------------------------------------
// getRandomSongs — pure shuffle. Pass straight through to Navidrome.
// The actual "radio from this song" feature is getSimilarSongs2.
// (The C#'s old getRandomSongs hijack, `GetRandomSongs_DISABLED_HIJACK`, was dead code and is
// not ported.)
// ---------------------------------------------------------------------
pub async fn get_random_songs(State(state): State<AppState>, req: Request) -> Result<Response, AppError> {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    // No catch: a relay failure is the global handler's (502/503 JSON).
    let passthrough = call.proxy.relay("rest/getRandomSongs", &call.parameters).await?;
    Ok(SubsonicReply::content(
        String::from_utf8_lossy(&passthrough.body).into_owned(),
        passthrough.content_type.as_deref().unwrap_or("application/json"),
    )
    .into_response())
}
