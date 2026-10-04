//! The catch-all `/{**endpoint}` (`SubsonicController.GenericEndpoint`, L3682): every method,
//! every path no route claims.
//!
//! STUB(wave 3-E/6-A): replaced when the Subsonic controller lands. Only the answers that need
//! no Navidrome are reproduced here:
//!
//! - `/` has no `endpoint`, which the implicit `[Required]` on a non-nullable `string` turns
//!   into the automatic validation 400 before the action runs;
//! - Octo-owned paths (lower-cased path starting with `admin` (which also matches
//!   `administrator`...) or `api/admin`, starting with `assets/`, or equal to `favicon.ico`)
//!   get `NotFound()`, the ProblemDetails 404, and are never relayed.
//!
//! Everything else would be relayed to Navidrome; until the relay is ported it answers 501.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::error::{problem, validation_problem};

pub async fn catch_all(req: Request) -> Response {
    let endpoint = req.uri().path().trim_start_matches('/');
    if endpoint.is_empty() {
        return validation_problem(&[("endpoint", &["The endpoint field is required."])]);
    }
    if is_octo_owned(endpoint) {
        return problem(StatusCode::NOT_FOUND);
    }
    // TODO(wave 3-E/6-A): native radio, the external-id safety net, the native catalog
    // answers and the faithful relay (endpoints.md §3.9).
    (
        StatusCode::NOT_IMPLEMENTED,
        "Octo (Rust) cannot relay to Navidrome yet: the relay is not ported.",
    )
        .into_response()
}

/// Paths Octo answers itself and never relays (step 1 of `GenericEndpoint`).
pub fn is_octo_owned(endpoint: &str) -> bool {
    let lower = endpoint.to_lowercase();
    lower.starts_with("admin")
        || lower.starts_with("api/admin")
        || lower.starts_with("assets/")
        || lower == "favicon.ico"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octo_owned_paths_are_matched_as_the_controller_matched_them() {
        for (endpoint, owned) in [
            ("admin", true),
            ("administrator", true),
            ("Admin/nope.js", true),
            ("api/admin/whatever", true),
            ("API/ADMINISTRATOR", true),
            ("assets/x.png", true),
            ("Assets", false),
            ("favicon.ico", true),
            ("FAVICON.ICO", true),
            ("favicon.ico/x", false),
            ("rest/ping", false),
            ("api/song", false),
        ] {
            assert_eq!(is_octo_owned(endpoint), owned, "{endpoint}");
        }
    }
}
