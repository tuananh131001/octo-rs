//! The catch-all `/{**endpoint}` (`SubsonicController.GenericEndpoint`, L3682; endpoints.md
//! §3.9): every method, every path no route claims.
//!
//! Ported so far (3-E): the answers that need no catalog, and the plain faithful relay.
//!
//! - `/` has no `endpoint`, which the implicit `[Required]` on a non-nullable `string` turns
//!   into the automatic validation 400 before the action runs;
//! - Octo-owned paths (lower-cased path starting with `admin` (which also matches
//!   `administrator`...) or `api/admin`, starting with `assets/`, or equal to `favicon.ico`)
//!   get `NotFound()`, the ProblemDetails 404, and are never relayed;
//! - everything else is relayed faithfully to Navidrome (step 11).
//!
//! TODO(6-A): steps 2-10 of §3.9 run before the relay and are not ported yet: native radio
//! (`TryServeNativeRadioAsync`), the external-id safety net (`HasExternalId` → the synthetic
//! empty ok of `ElementFor(endpoint)`), and the native catalog answers for external songs,
//! albums and artists and the native song and album search injections.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use octo_subsonic::subsonic_request_parser::unescape_data_string;

use super::error::{problem, validation_problem};
use crate::app::AppState;
use crate::services::subsonic::{IncomingRequest, RawRelayResult};
use octo_subsonic::SubsonicResponseBuilder;

/// Kestrel's `MaxRequestBodySize` default.
const MAX_REQUEST_BODY: usize = 30_000_000;

pub async fn catch_all(State(state): State<AppState>, req: Request) -> Response {
    let endpoint = endpoint_of(req.uri().path());
    if endpoint.is_empty() {
        return validation_problem(&[("endpoint", &["The endpoint field is required."])]);
    }
    if is_octo_owned(&endpoint) {
        return problem(StatusCode::NOT_FOUND);
    }

    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let incoming = Arc::new(IncomingRequest::from_parts(&parts, body));
    let parameters = incoming.parameters();
    let format = parameters.get("f").map_or("xml", String::as_str).to_string();

    // TODO(6-A): TryServeNativeRadioAsync, the HasExternalId safety net and the native
    // external song/album/artist answers and search injections (§3.9 steps 2-10) go here.

    // Faithful relay: forward the caller's method + body + status so native Navidrome
    // endpoints (e.g. the POST /auth/login some clients use) work, not just GET-shaped
    // Subsonic calls.
    let relay = state.subsonic_proxy.with_request(Arc::clone(&incoming));
    match relay.relay_raw(&endpoint, &parameters).await {
        Ok(raw) => {
            // Learn Octo's own Navidrome identity from a client's native sign-in as it passes
            // through, so background work (music-folder detection, an authenticated rescan)
            // has an admin token without any extra config.
            if raw.status == 200 && endpoint.eq_ignore_ascii_case("auth/login") {
                state.navidrome_identity.capture_login(&raw.body);
            }
            write_raw_relay(raw, &format, &state.subsonic_response_builder)
        }
        Err(error) => state
            .subsonic_response_builder
            .create_error(
                &format,
                0,
                &format!("Error connecting to Subsonic server: {error}"),
            )
            .into_response(),
    }
}

/// The route value `{**endpoint}` held: the path without its leading `/`, decoded as Kestrel
/// decoded paths (every `%XX` but `%2F`, which stays encoded), in the client's casing.
pub fn endpoint_of(path: &str) -> String {
    let path = path.strip_prefix('/').unwrap_or(path);
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(at) = find_encoded_slash(rest) {
        out.push_str(&unescape_data_string(&rest[..at]));
        out.push_str(&rest[at..at + 3]);
        rest = &rest[at + 3..];
    }
    out.push_str(&unescape_data_string(rest));
    out
}

fn find_encoded_slash(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    lower.find("%2f")
}

/// Paths Octo answers itself and never relays (step 1 of `GenericEndpoint`).
pub fn is_octo_owned(endpoint: &str) -> bool {
    let lower = octo_core::common::dotnet::to_lower_invariant(endpoint);
    lower.starts_with("admin")
        || lower.starts_with("api/admin")
        || lower.starts_with("assets/")
        || lower == "favicon.ico"
}

/// What the C# wrote for a faithful relay: the upstream status, the allowlisted headers (each
/// `Response.Headers[name] = value`, so the last value of a name wins), the upstream content
/// type or `application/{f}`, and the body written to the response stream, which Kestrel sent
/// chunked.
fn write_raw_relay(raw: RawRelayResult, format: &str, builder: &SubsonicResponseBuilder) -> Response {
    let status = StatusCode::from_u16(raw.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = raw
        .content_type
        .clone()
        .unwrap_or_else(|| format!("application/{format}"));
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in &raw.response_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    let Ok(content_type) = HeaderValue::from_str(&content_type) else {
        // Kestrel refused the header when the response started, inside the C#'s try: the
        // error envelope went out with the status and headers already set.
        let mut response = builder
            .create_error(
                format,
                0,
                "Error connecting to Subsonic server: Invalid non-ASCII or control character in header.",
            )
            .into_response();
        *response.status_mut() = status;
        response.headers_mut().extend(headers);
        return response;
    };
    headers.insert(header::CONTENT_TYPE, content_type);

    let body = if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED {
        Body::empty()
    } else {
        // A stream, so hyper frames it chunked as Kestrel did for a body written without a
        // Content-Length.
        let chunk: Result<Bytes, std::io::Error> = Ok(raw.body);
        Body::from_stream(futures::stream::iter([chunk]))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
#[path = "catch_all_tests.rs"]
mod tests;
