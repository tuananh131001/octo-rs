//! Port of `Controllers/CoverUpgradeController.cs`: the dashboard's "Upgrade cover art", which
//! finds the largest cover for every album and embeds it. It rewrites the owner's files, so it
//! needs a Navidrome admin sign-in like the genre backfill, and a whole-library run (not a
//! preview) needs the music path typed back.
//!
//! Every route checks the session with `Validate`, which slides the server-side expiry but, unlike
//! `AdminController`'s `BrowseUser`, does not re-issue the cookie.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::helpers_6b2::{
    bind_body, enum_try_parse, error, ok, query_bool, sign_in, signed, split, status, utc_opt,
};
use crate::app::AppState;
use crate::http::error::problem;
use crate::http::routes::RouteSet;
use crate::services::cover_art::{
    CoverUpgradeMode, CoverUpgradeRequest, CoverUpgradeScope, CoverUpgradeStatus, CoverUpgradeWorker,
};

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/covers/upgrade", get(get_run).post(start))
        .route("/api/admin/covers/upgrade/cancel", post(cancel))
        .route("/api/admin/covers/upgrade/resume", post(resume))
        .route("/api/admin/covers/upgrade/thumb/{id}", get(thumb))
        .route("/api/admin/covers/upgrade/undo", post(undo))
}

const SCOPES: [&str; 2] = ["OctoDownloads", "WholeLibrary"];
const MODES: [&str; 3] = ["Scan", "Preview", "Apply"];
const ALREADY_RUNNING: &str = "A cover upgrade is already running.";

fn scope_name(scope: CoverUpgradeScope) -> &'static str {
    match scope {
        CoverUpgradeScope::OctoDownloads => "OctoDownloads",
        CoverUpgradeScope::WholeLibrary => "WholeLibrary",
    }
}

fn mode_name(mode: CoverUpgradeMode) -> &'static str {
    MODES[mode as usize]
}

fn status_name(status: CoverUpgradeStatus) -> &'static str {
    match status {
        CoverUpgradeStatus::Idle => "Idle",
        CoverUpgradeStatus::Running => "Running",
        CoverUpgradeStatus::Completed => "Completed",
        CoverUpgradeStatus::Cancelled => "Cancelled",
        CoverUpgradeStatus::Interrupted => "Interrupted",
        CoverUpgradeStatus::Failed => "Failed",
    }
}

/// The folder a whole-library run walks: Navidrome's music folder when it was detected, else
/// `Library:DownloadPath` (or /music).
fn music_path(state: &AppState) -> String {
    let fallback = state
        .settings
        .raw("Library:DownloadPath")
        .unwrap_or_else(|| "/music".to_string());
    state.navidrome_identity.effective_download_path(&fallback)
}

/// `StartRequest(string? Scope, string? Mode, bool FolderCovers = true, int SmallerThan =
/// DefaultSmallerThan, List<string>? Albums = null, string? Confirm = null)`.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct StartRequest {
    scope: Option<String>,
    mode: Option<String>,
    folder_covers: bool,
    smaller_than: i32,
    albums: Option<Vec<String>>,
    confirm: Option<String>,
}

impl Default for StartRequest {
    fn default() -> Self {
        StartRequest {
            scope: None,
            mode: None,
            folder_covers: true,
            smaller_than: CoverUpgradeWorker::DEFAULT_SMALLER_THAN,
            albums: None,
            confirm: None,
        }
    }
}

async fn get_run(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let worker = &state.cover_upgrade_worker;
    let run = worker.current();
    // Without each album's file list, which only the next run needs.
    let preview: Vec<Value> = run
        .preview
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "folder": row.folder,
                "artist": row.artist,
                "album": row.album,
                "fromSide": row.from_side,
                "toSide": row.to_side,
                "source": row.source,
                "files": row.files,
                "folderCover": row.folder_cover,
                "result": row.result,
                "looksSame": row.looks_same,
            })
        })
        .collect();
    ok(&json!({
        "runId": run.run_id,
        "status": status_name(run.status),
        "scope": scope_name(run.scope),
        "mode": mode_name(run.mode),
        "dryRun": run.dry_run(),
        "smallerThan": run.smaller_than,
        "picked": run.selected.as_ref().map(Vec::len),
        "soft": run.soft,
        "folderCovers": run.folder_covers,
        "fullSize": run.full_size,
        "undo": run.undo,
        "startedUtc": utc_opt(&run.started_utc),
        "finishedUtc": utc_opt(&run.finished_utc),
        "total": run.total,
        "processed": run.processed,
        "upgraded": run.upgraded,
        "kept": run.kept,
        "files": run.files,
        "failed": run.failed,
        "lastFolder": run.last_folder,
        "reason": run.reason,
        "songsTotal": run.songs_total,
        "songsRead": run.songs_read,
        "albumsTotal": run.albums_total,
        "albumsDone": run.albums_done,
        "errors": run.errors,
        "preview": preview,
        "canResume": run.can_resume(),
        // Accepted and not yet started, or started: the dashboard watches while this is true.
        "busy": worker.is_busy(),
        "canUndo": worker.can_undo(),
        "musicPath": music_path(&state),
    }))
}

async fn start(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: StartRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    if !signed(&state, &parts) {
        return sign_in();
    }
    let scope = match enum_try_parse(&SCOPES, request.scope.as_deref()) {
        Some(1) => CoverUpgradeScope::WholeLibrary,
        _ => CoverUpgradeScope::OctoDownloads,
    };
    let mode = match enum_try_parse(&MODES, request.mode.as_deref()) {
        Some(1) => CoverUpgradeMode::Preview,
        Some(2) => CoverUpgradeMode::Apply,
        _ => CoverUpgradeMode::Scan,
    };
    let smaller_than = request.smaller_than.clamp(1, 10_000);
    if request.albums.as_ref().is_some_and(Vec::is_empty) {
        return error(StatusCode::BAD_REQUEST, "Pick at least one album.");
    }

    // Picked albums came off a list the admin was shown; a run over everything in the whole
    // library is the one that needs the path typed back.
    let root = music_path(&state);
    if scope == CoverUpgradeScope::WholeLibrary
        && mode == CoverUpgradeMode::Apply
        && request.albums.is_none()
        && request.confirm.as_deref().map(str::trim) != Some(root.as_str())
    {
        return error(
            StatusCode::BAD_REQUEST,
            format!("To rewrite the whole library, type the music path exactly: {root}"),
        );
    }

    let picked = match &request.albums {
        None => "every album".to_string(),
        Some(albums) => format!("{} picked", albums.len()),
    };
    let mut run = CoverUpgradeRequest::new(scope, mode, request.folder_covers);
    run.smaller_than = smaller_than;
    run.albums = request.albums;
    if !state.cover_upgrade_worker.try_enqueue(run) {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    info!(
        "Cover upgrade requested: {}, scope {}, under {smaller_than} px, folder covers {}, {picked}",
        mode_name(mode),
        scope_name(scope),
        if request.folder_covers { "True" } else { "False" }
    );
    status(StatusCode::ACCEPTED, &json!({ "started": true }))
}

async fn cancel(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    state.cover_upgrade_worker.request_cancel();
    status(StatusCode::ACCEPTED, &json!({ "cancelling": true }))
}

async fn resume(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let run = state.cover_upgrade_worker.current();
    if !run.can_resume() {
        return error(StatusCode::BAD_REQUEST, "There is nothing to resume.");
    }
    let mut again = CoverUpgradeRequest::new(run.scope, run.mode, run.folder_covers);
    again.smaller_than = run.smaller_than;
    again.albums = run.selected;
    if !state.cover_upgrade_worker.try_enqueue(again) {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    status(StatusCode::ACCEPTED, &json!({ "resumed": true }))
}

/// `File(bytes, type)`.
fn file(bytes: Vec<u8>, content_type: &str) -> Response {
    let mut response = Response::new(Body::from(bytes));
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(CONTENT_TYPE, value);
    }
    response
}

/// The cover an album on the list has now, small, so the soft ones can be seen; with
/// `found=true`, the larger one a preview found for it.
async fn thumb(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    // `[FromQuery] bool found`: a value that is not a bool is the binder's 400, before the action.
    let found = match query_bool(&parts.uri, "found") {
        Ok(found) => found,
        Err(answer) => return *answer,
    };
    if !signed(&state, &parts) {
        return sign_in();
    }
    let mut response = thumb_answer(&state, &id, found).await;
    // Set before the action chose its answer, so a 404 carries it too.
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
    response
}

async fn thumb_answer(state: &AppState, id: &str, found: bool) -> Response {
    let worker: Arc<CoverUpgradeWorker> = state.cover_upgrade_worker.clone();
    // The cover Navidrome shows, already made at the apps' size; reading the song is the
    // fallback, and over a network mount it is the slow one.
    if !found && let Some((bytes, content_type)) = worker.navidrome_thumbnail(id).await {
        return file(bytes, &content_type);
    }
    let id = id.to_string();
    let bytes = tokio::task::spawn_blocking(move || {
        if found {
            worker.found_thumbnail(&id)
        } else {
            worker.thumbnail(&id)
        }
    })
    .await
    .ok()
    .flatten();
    match bytes {
        Some(bytes) => {
            let content_type = octo_media::cover::cover_image::mime_type(&bytes);
            file(bytes, content_type)
        }
        None => problem(StatusCode::NOT_FOUND),
    }
}

async fn undo(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    if !state.cover_upgrade_worker.can_undo() {
        return error(StatusCode::BAD_REQUEST, "There is no cover upgrade to undo.");
    }
    if !state
        .cover_upgrade_worker
        .try_enqueue(CoverUpgradeRequest::undo())
    {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    info!("Cover upgrade undo requested");
    status(StatusCode::ACCEPTED, &json!({ "started": true }))
}
