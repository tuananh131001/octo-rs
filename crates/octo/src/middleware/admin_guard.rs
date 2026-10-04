//! The admin API has no login, so the browser's same-origin policy is the only thing between it
//! and any web page a LAN user happens to visit. Octo's CORS policy allows every origin, which
//! Subsonic web players need, and it used to apply to /api/admin as well: a page anywhere could
//! read every stored key and password and post new settings.
//!
//! Reads: CORS headers are stripped, so a browser on another origin cannot read the answer.
//! Writes: must carry X-Octo-Admin. A page on another origin cannot add a custom header without
//! a preflight, and the preflight is answered without CORS approval, so the write never leaves
//! the browser. A script (curl, Home Assistant) can send the header deliberately.
//!
//! This is not authentication. Anyone who can reach the port directly, or a DNS-rebinding page
//! that makes itself look same-origin, still gets through. /admin and /api/admin must stay off
//! the internet.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::http::error::json_relaxed_response;

pub const HEADER_NAME: &str = "X-Octo-Admin";

/// Whether a path is `/api/admin` or below it, as `PathString.StartsWithSegments` decides:
/// case-insensitive, and only at a segment boundary.
pub fn is_admin_path(path: &str) -> bool {
    let prefix = "/api/admin";
    path.len() >= prefix.len()
        && path[..prefix.len()].eq_ignore_ascii_case(prefix)
        && (path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
}

/// Runs outside the CORS layer, so it sees (and strips) the headers CORS added on the way out.
pub async fn admin_request_guard(req: Request, next: Next) -> Response {
    if !is_admin_path(req.uri().path()) {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let mut res = if method == Method::GET || method == Method::HEAD {
        next.run(req).await
    } else if method == Method::OPTIONS {
        // A preflight answered without Access-Control-Allow-* is a refusal.
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NO_CONTENT;
        r
    } else if !req.headers().contains_key(HEADER_NAME) {
        // WriteAsJsonAsync: the relaxed encoder, so the apostrophe goes out as is.
        json_relaxed_response(
            StatusCode::FORBIDDEN,
            &json!({
                "error": format!(
                    "Admin changes must come from Octo's dashboard. A script can send the {HEADER_NAME} header to opt in."
                ),
            }),
            "application/json; charset=utf-8",
        )
        .into_response()
    } else {
        next.run(req).await
    };
    let strip: Vec<_> = res
        .headers()
        .keys()
        .filter(|k| k.as_str().to_ascii_lowercase().starts_with("access-control-"))
        .cloned()
        .collect();
    for k in strip {
        res.headers_mut().remove(k);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_paths_match_at_segment_boundaries_ignoring_case() {
        assert!(is_admin_path("/api/admin"));
        assert!(is_admin_path("/API/Admin/settings"));
        assert!(!is_admin_path("/api/administrator"));
        assert!(!is_admin_path("/rest/ping"));
    }
}
