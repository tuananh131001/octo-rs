//! `AdminController`'s library actions, questions and checks (L1137-L1250, L1433): the action
//! history, the notices, the duplicate scan, the library Review sweep, the weekly upgrade's
//! status and the song path resolver.
//!
//! Session-gated, because these list filenames and usernames. Read-only: there is deliberately
//! no endpoint here that deletes a quarantined file or applies an action on demand, since
//! /api/admin has no authentication of its own and those would be the wrong things to leave
//! reachable.

use std::path::Path;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::helpers_6b2::{browse_user, error, ok, query, sign_in, status, utc, utc_opt};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::library::library_action_journal::action_name;
use crate::services::library::navidrome_song_path_resolver::PathSource;
use crate::services::library::notice_queue::NamedEnum;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/library-actions", get(get_library_actions))
        .route("/api/admin/notices", get(get_notices))
        .route("/api/admin/duplicates/scan", post(scan_duplicates))
        .route("/api/admin/review-sweep", get(get_review_sweep))
        .route("/api/admin/review-sweep/start", post(start_review_sweep))
        .route("/api/admin/review-sweep/pause", post(pause_review_sweep))
        .route("/api/admin/review-sweep/reset", post(reset_review_sweep))
        .route("/api/admin/quality-upgrade", get(get_quality_upgrade))
        .route("/api/admin/library/resolve", get(resolve_library_song))
}

/// `Accepted(new { ... })`.
fn accepted(body: &Value) -> Response {
    status(StatusCode::ACCEPTED, body)
}

/// `PathSource.ToString()`.
pub fn path_source_name(source: PathSource) -> &'static str {
    match source {
        PathSource::NativeApi => "NativeApi",
        PathSource::SubsonicGetSong => "SubsonicGetSong",
        PathSource::LocalMappings => "LocalMappings",
        PathSource::None => "None",
    }
}

/// `Path.Combine(root, relative)`: a rooted second part replaces the first.
fn path_combine(root: &str, relative: &str) -> String {
    if relative.starts_with('/') || root.is_empty() {
        return relative.to_string();
    }
    Path::new(root).join(relative).to_string_lossy().into_owned()
}

/// The library actions history, the 200 most recent.
async fn get_library_actions(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let settings = state.settings.current().library_actions.clone();
    let entries: Vec<Value> = state
        .library_action_journal
        .recent(200)
        .iter()
        .map(|entry| {
            json!({
                "action": action_name(entry.action),
                "navidromeId": entry.navidrome_id,
                "username": entry.username,
                "title": entry.title,
                "artist": entry.artist,
                "album": entry.album,
                "state": entry.state.name(),
                "detail": entry.detail,
                "dryRun": entry.dry_run,
                "sourcePath": entry.source_path,
                "quarantinePath": entry.quarantine_path,
                "resolution": entry.resolution.map(path_source_name),
                "atUtc": utc(&entry.at_utc),
            })
        })
        .collect();
    browse.finish(ok(&json!({
        "enabled": settings.enabled,
        "dryRun": settings.dry_run,
        "hasAdminIdentity": state.navidrome_identity.has_admin_identity(),
        "allowedUsers": settings.allowed_users,
        "quarantine": path_combine(
            &state.navidrome_song_path_resolver.music_root(),
            &settings.effective_quarantine_directory()
        ),
        "entries": entries,
    })))
}

/// What Octo has asked people about (#47, #53) and how they answered, newest first. Gated like
/// the action history: it names files and people.
async fn get_notices(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let entries: Vec<Value> = state
        .notice_queue
        .recent(200)
        .iter()
        .map(|entry| {
            json!({
                "kind": entry.kind.name(),
                "username": entry.username,
                "artist": entry.artist,
                "title": entry.title,
                "album": entry.album,
                "state": entry.state.name(),
                "reason": entry.reason,
                "origin": entry.origin.name(),
                "submitted": entry.submitted,
                "createdUtc": utc(&entry.created_utc),
                "resolvedUtc": utc_opt(&entry.resolved_utc),
            })
        })
        .collect();
    let duplicate_scan = state.duplicate_scan_worker.last_result().map(|r| {
        json!({
            "atUtc": utc(&r.at_utc),
            "tracks": r.tracks,
            "groups": r.groups,
            "added": r.added,
            "complete": r.complete,
        })
    });
    browse.finish(ok(&json!({
        "entries": entries,
        "duplicateScan": duplicate_scan,
    })))
}

/// The Duplicates card's "Scan now". Read-only, like a radio refresh, so the admin request
/// guard is enough; the walk runs in the background and the result shows with the questions.
async fn scan_duplicates(State(state): State<AppState>) -> Response {
    let settings = state.settings.current().library_actions.clone();
    if !settings.enabled || !settings.duplicates_enabled {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on library actions and the Duplicates playlist first.",
        );
    }
    if !state.navidrome_identity.has_admin_identity() {
        return error(
            StatusCode::BAD_REQUEST,
            "Octo needs a Navidrome admin credential to read the whole library.",
        );
    }
    state.duplicate_scan_worker.request_scan();
    accepted(&json!({ "ok": true, "queued": true }))
}

/// The library Review sweep (#72): how far it has got and why it is waiting. Counts only.
async fn get_review_sweep(State(state): State<AppState>) -> Response {
    let s = state.library_review_sweep_worker.status();
    ok(&json!({
        "state": s.state,
        "reason": s.reason,
        "paused": s.paused,
        "position": s.position,
        "total": s.total,
        "pass": s.pass,
        "found": s.found,
        "open": s.open,
        "fine": s.fine,
        "undecodable": s.undecodable,
        "keeper": s.keeper,
        "perHour": s.per_hour,
        "lastCheckedUtc": utc_opt(&s.last_checked_utc),
        "passFinishedUtc": utc_opt(&s.pass_finished_utc),
    }))
}

async fn start_review_sweep(State(state): State<AppState>) -> Response {
    let settings = state.settings.current().library_actions.clone();
    if !settings.enabled || !settings.review_enabled || settings.effective_review_sweep_per_hour() == 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on library actions and Review, and set how many songs an hour to check, first.",
        );
    }
    state.library_review_sweep_worker.set_paused(false);
    accepted(&json!({ "ok": true }))
}

async fn pause_review_sweep(State(state): State<AppState>) -> Response {
    state.library_review_sweep_worker.set_paused(true);
    accepted(&json!({ "ok": true }))
}

/// Check every song again from the start. Songs already asked about stay answered.
async fn reset_review_sweep(State(state): State<AppState>) -> Response {
    state.library_review_sweep_worker.reset();
    accepted(&json!({ "ok": true }))
}

/// The weekly upgrade's last run and next one. Times and an outcome only, no file or person, so
/// the admin request guard is enough, like the duplicate scan.
async fn get_quality_upgrade(State(state): State<AppState>) -> Response {
    let s = state.quality_upgrade_worker.status();
    ok(&json!({
        "perWeek": s.per_week,
        "off": s.off,
        "lastRunUtc": utc_opt(&s.last_run_utc),
        "lastOutcome": s.last_outcome,
        "nextDueUtc": utc_opt(&s.next_due_utc),
        "tried": s.tried,
    }))
}

/// Ask what file a Navidrome song id resolves to, and say which leg answered.
///
/// Read-only, and deliberately shipped before anything that acts on the answer: Navidrome's
/// Subsonic `path` is synthesised from tags unless a player opts in, so on some libraries it
/// names a file that exists and is a different recording. This is how that gets checked against
/// a real library before any of it is load-bearing.
///
/// Session-gated because the answer is a filesystem path.
async fn resolve_library_song(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let Some(id) = query(&parts.uri, "id").filter(|id| !id.trim().is_empty()) else {
        return browse.finish(error(
            StatusCode::BAD_REQUEST,
            "Pass the Navidrome song id as ?id=",
        ));
    };
    let resolved = state.navidrome_song_path_resolver.resolve(&id).await;
    browse.finish(ok(&json!({
        "id": id,
        "resolved": resolved.is_some(),
        "musicRoot": state.navidrome_song_path_resolver.music_root(),
        "hasAdminIdentity": state.navidrome_identity.has_admin_identity(),
        "path": resolved.as_ref().map(|r| r.absolute_path.clone()),
        "source": path_source_name(resolved.as_ref().map_or(PathSource::None, |r| r.source)),
        "sizeBytes": resolved.as_ref().map(|r| r.size_bytes),
        "artist": resolved.as_ref().map(|r| r.artist.clone()),
        "title": resolved.as_ref().map(|r| r.title.clone()),
        "album": resolved.as_ref().map(|r| r.album.clone()),
    })))
}
