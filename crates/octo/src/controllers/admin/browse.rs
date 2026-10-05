//! `AdminController`'s browse sign-in and the endpoints that read files (L439–L637): the
//! Navidrome admin sign-in (`browse/auth`), the folder picker (`browse`), who is signed in
//! (`browse/session`), signing out, and the tag preview.

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use octo_core::common::dotnet::is_blank;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::helpers_6b1::{
    BROWSE_COOKIE_NAME, BROWSE_TOKEN_HEADER, BrowseUser, bind_body, browse_cookie, browse_cookie_deleted,
    camel_case_value, error_json, header_value, is_https, query_value, request_cookie,
};
use crate::app::AppState;
use crate::http::error::json_ok;
use crate::middleware::forwarded::RequestScheme;

/// Credentials for `browse/auth`. Body-only by design.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BrowseAuthRequest {
    pub username: Option<String>,
    pub password: Option<String>,
}

/// `POST /api/admin/browse/auth`: exchanges Navidrome admin credentials for a browse session.
///
/// Credentials arrive in the body, never the query string, so they cannot end up in access
/// logs or a referrer. Verification is delegated to the Navidrome Octo already fronts: no new
/// credential store, and admin rights are Navidrome's call rather than something Octo asserts
/// for itself.
pub async fn browse_auth(
    State(state): State<AppState>,
    scheme: Option<Extension<RequestScheme>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: BrowseAuthRequest = match bind_body(&headers, &body, "req") {
        Ok(request) => request,
        Err(res) => return res,
    };
    let url = state
        .settings
        .current()
        .subsonic
        .url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    if url.is_empty() {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "Navidrome URL is not configured yet.",
        );
    }
    let username = request.username.unwrap_or_default();
    let password = request.password.unwrap_or_default();
    if is_blank(&username) || is_blank(&password) {
        return error_json(StatusCode::UNAUTHORIZED, "Username and password are required.");
    }

    let payload = octo_core::json::to_string(&json!({ "username": username, "password": password }));
    let reply = state
        .http
        .post(format!("{url}/auth/login"))
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(payload)
        .send()
        .await;
    let outcome: Result<Option<&str>, String> = match reply {
        Err(e) => Err(crate::services::http_client_factory::connect_failure_message(&e)),
        Ok(response) if !response.status().is_success() => Ok(Some("Navidrome rejected those credentials.")),
        Ok(response) => match response.bytes().await {
            Err(e) => Err(e.to_string()),
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Err(e) => Err(e.to_string()),
                Ok(document) => {
                    let is_admin = document.get("isAdmin") == Some(&serde_json::Value::Bool(true));
                    Ok((!is_admin).then_some("That account is not a Navidrome admin."))
                }
            },
        },
    };
    match outcome {
        Err(message) => {
            warn!("Browse auth against Navidrome failed: {message}");
            error_json(
                StatusCode::BAD_GATEWAY,
                "Could not reach Navidrome to verify credentials.",
            )
        }
        Ok(Some(refusal)) => error_json(StatusCode::UNAUTHORIZED, refusal),
        Ok(None) => {
            info!("Browse session opened for Navidrome admin {username}");
            let token = state.browse_sessions.create(&username);
            // Hand the session back as an HttpOnly cookie rather than something the page has
            // to hold. It survives a reload, so the user is not asked to sign in again every
            // time they come back to the settings, and script on the page cannot read it even
            // if something managed to inject some.
            let mut res = json_ok(&json!({ "ok": true, "user": username }));
            res.headers_mut().append(
                header::SET_COOKIE,
                browse_cookie(&token, is_https(scheme.as_deref())),
            );
            res
        }
    }
}

/// `GET /api/admin/browse?path=`: lists directories so the download folder can be picked
/// rather than typed. Requires a browse session; a 401 discloses nothing about the filesystem,
/// not even whether a path exists.
pub async fn browse(
    State(state): State<AppState>,
    scheme: Option<Extension<RequestScheme>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    // Cookie first (how the admin UI authenticates), header second so the endpoint stays
    // usable from curl or a script without one.
    let token = header_value(&headers, BROWSE_TOKEN_HEADER);
    let signed_in = BrowseUser::check(&state, &headers, scheme.as_deref(), token.as_deref());
    if !signed_in.signed_in() {
        return error_json(StatusCode::UNAUTHORIZED, "Browse session required.");
    }
    let path = query_value(query.as_deref(), "path");
    let result = state.directory_browser.browse(path.as_deref());
    let mut answer = serde_json::to_value(&result).unwrap_or_default();
    if let Some(map) = answer.as_object_mut() {
        // Under Docker this is the container's mount namespace, not the host's drives. Saying
        // so in the payload keeps the UI honest about why a user's D: drive is nowhere to be
        // seen. (`Directory.Exists`: /.dockerenv is a file, so this is false in practice.)
        map.insert(
            "containerised".into(),
            serde_json::Value::Bool(std::path::Path::new("/.dockerenv").is_dir()),
        );
    }
    signed_in.finish(json_ok(&answer))
}

/// `GET /api/admin/browse/session`: who this browser is signed in as, for the dashboard's
/// footer. Never prompts. The cookie alone counts here.
pub async fn browse_session(
    State(state): State<AppState>,
    scheme: Option<Extension<RequestScheme>>,
    headers: HeaderMap,
) -> Response {
    let signed_in = BrowseUser::check(&state, &headers, scheme.as_deref(), None);
    let answer = match &signed_in.user {
        Some(user) => json_ok(&json!({ "signedIn": true, "user": user })),
        None => json_ok(&json!({ "signedIn": false })),
    };
    signed_in.finish(answer)
}

/// `POST /api/admin/browse/signout`: signs this browser out; the session is forgotten and the
/// cookie removed.
pub async fn browse_sign_out(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state
        .browse_sessions
        .revoke(request_cookie(&headers, BROWSE_COOKIE_NAME).as_deref());
    let mut res = json_ok(&json!({ "ok": true }));
    res.headers_mut()
        .append(header::SET_COOKIE, browse_cookie_deleted());
    res
}

/// A file inside the music folder, or an artist and a title, to try the matching on.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TagPreviewRequest {
    pub path: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
}

/// `POST /api/admin/tags/preview`: how a song would be matched and tagged, without touching
/// anything. Gated on the browse sign-in like the other endpoints that read files, and a path
/// is only ever a file inside the music folder, resolved in full, with no link on the way.
pub async fn preview_tags(
    State(state): State<AppState>,
    scheme: Option<Extension<RequestScheme>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: TagPreviewRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    let token = header_value(&headers, BROWSE_TOKEN_HEADER);
    let signed_in = BrowseUser::check(&state, &headers, scheme.as_deref(), token.as_deref());
    if !signed_in.signed_in() {
        return error_json(
            StatusCode::UNAUTHORIZED,
            "Sign in with your Navidrome admin account first.",
        );
    }

    let mut path = None;
    if let Some(candidate) = request.path.as_deref().filter(|p| !is_blank(p)) {
        let configured = state
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        let root = state.navidrome_identity.effective_download_path(&configured);
        path = resolve_under_root(candidate, &root);
        if path.is_none() {
            return signed_in.finish(error_json(
                StatusCode::BAD_REQUEST,
                "The path must be a file inside the music folder.",
            ));
        }
    } else if request.artist.as_deref().is_none_or(is_blank) || request.title.as_deref().is_none_or(is_blank)
    {
        return signed_in.finish(error_json(
            StatusCode::BAD_REQUEST,
            "Give a path inside the music folder, or an artist and a title.",
        ));
    }

    let report = state
        .tag_preview
        .preview(
            path.as_deref(),
            request.artist.as_deref(),
            request.title.as_deref(),
            request.album.as_deref(),
            &CancellationToken::new(),
        )
        .await;
    let answer = match report {
        Ok(report) => {
            let value = serde_json::to_value(&report).unwrap_or_default();
            json_ok(&camel_case_value(value, &["fields", "stageSeconds"]))
        }
        Err(_) => crate::http::error::problem(StatusCode::INTERNAL_SERVER_ERROR),
    };
    signed_in.finish(answer)
}

/// `Path.GetFullPath` on Unix: made absolute against the working folder, with `.`, `..`
/// (never above the root) and repeated separators worked out on the text alone.
fn full_path(path: &str) -> Option<String> {
    if path.contains('\0') {
        return None;
    }
    let combined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir().ok()?.to_string_lossy().into_owned();
        format!("{}/{path}", cwd.trim_end_matches('/'))
    };
    let trailing = combined.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in combined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut full = format!("/{}", parts.join("/"));
    if trailing && full != "/" && !combined.ends_with("/.") && !combined.ends_with("/..") {
        full.push('/');
    }
    Some(full)
}

/// `Path.GetRelativePath(relativeTo, path)` for two full Unix paths (case-sensitive).
fn relative_path(relative_to: &str, path: &str) -> String {
    let from: Vec<&str> = relative_to.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == from.len() && common == to.len() {
        return ".".to_string();
    }
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

fn is_link(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// The full path of a file that really sits under the root: no ".." out of it, no path of its
/// own, and no link on the way, so the preview cannot be pointed at any other file.
pub fn resolve_under_root(candidate: &str, root: &str) -> Option<String> {
    let full = full_path(candidate)?;
    let root_full = full_path(root)?;
    let root_full = root_full.trim_end_matches('/');
    let relative = relative_path(root_full, &full);
    if relative == "." || relative.starts_with("..") || relative.starts_with('/') {
        return None;
    }

    // FileInfo: a file (not a folder) that exists, and is not itself a link.
    let metadata = std::fs::symlink_metadata(&full).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let mut dir = std::path::Path::new(&full).parent();
    while let Some(d) = dir {
        let text = d.to_string_lossy();
        if text.len() <= root_full.len() {
            break;
        }
        if is_link(&text) {
            return None;
        }
        dir = d.parent();
    }
    Some(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{prefix}{}", uuid::Uuid::new_v4().simple()))
    }

    #[test]
    fn resolve_under_root_refuses_dot_dot_rooted_and_missing_accepts_a_file_inside() {
        let root = temp("octo-root-");
        std::fs::create_dir_all(root.join("Artist")).unwrap();
        let inside = root.join("Artist").join("song.flac");
        std::fs::write(&inside, [1u8]).unwrap();
        let outside =
            std::env::temp_dir().join(format!("octo-outside-{}.flac", uuid::Uuid::new_v4().simple()));
        std::fs::write(&outside, [1u8]).unwrap();
        let root_s = root.to_string_lossy().to_string();
        let inside_s = inside.to_string_lossy().to_string();

        assert_eq!(
            resolve_under_root(&inside_s, &root_s).as_deref(),
            Some(inside_s.as_str())
        );
        assert_eq!(
            resolve_under_root(&format!("{root_s}/Artist/../Artist/song.flac"), &root_s).as_deref(),
            Some(inside_s.as_str())
        );
        let name = outside.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(resolve_under_root(&format!("{root_s}/../{name}"), &root_s), None);
        assert_eq!(resolve_under_root(&outside.to_string_lossy(), &root_s), None);
        assert_eq!(
            resolve_under_root(&format!("{root_s}/Artist/missing.flac"), &root_s),
            None
        );
        assert_eq!(resolve_under_root(&root_s, &root_s), None);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    /// A link inside the folder that points outside is refused.
    #[test]
    fn resolve_under_root_refuses_a_symlink() {
        let root = temp("octo-root-");
        std::fs::create_dir_all(&root).unwrap();
        let outside =
            std::env::temp_dir().join(format!("octo-outside-{}.flac", uuid::Uuid::new_v4().simple()));
        std::fs::write(&outside, [1u8]).unwrap();
        let link = root.join("link.flac");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(
            resolve_under_root(&link.to_string_lossy(), &root.to_string_lossy()),
            None
        );

        // A linked folder on the way is refused too.
        let real_dir = temp("octo-real-");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(real_dir.join("song.flac"), [1u8]).unwrap();
        std::os::unix::fs::symlink(&real_dir, root.join("linked")).unwrap();
        assert_eq!(
            resolve_under_root(
                &root.join("linked/song.flac").to_string_lossy(),
                &root.to_string_lossy()
            ),
            None
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&real_dir);
        let _ = std::fs::remove_file(&outside);
    }
}
