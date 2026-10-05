//! `AdminController`'s genre normalisation (L1468-L1609): the backfill over the library's tags,
//! its undo, and the broad-genre preset.
//!
//! /api/admin has no authentication at all, so a button that rewrites every tag in a music
//! library cannot be the second unauthenticated destructive surface: every backfill endpoint is
//! gated on a verified Navidrome admin session. The preset is read-only and stays open.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use octo_core::settings::{GenreMatchMode, GenreSettings};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::helpers_6b2::{
    bind_body, browse_user, enum_try_parse, error, ok, sign_in, split, status, utc_opt,
};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::metadata::genre_backfill_state::{GenreBackfillScope, GenreBackfillStatus};
use crate::services::metadata::{GenreBackfillRequest, GenreBackfillWorker};

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route(
            "/api/admin/genre/backfill",
            get(get_genre_backfill).post(start_genre_backfill),
        )
        .route("/api/admin/genre/backfill/cancel", post(cancel_genre_backfill))
        .route("/api/admin/genre/backfill/resume", post(resume_genre_backfill))
        .route("/api/admin/genre/backfill/undo", post(undo_genre_backfill))
        .route("/api/admin/genre/presets", get(get_genre_presets))
}

const SCOPES: [&str; 2] = ["OctoDownloads", "WholeLibrary"];
const ALREADY_RUNNING: &str = "A genre backfill is already running.";

fn scope_name(scope: GenreBackfillScope) -> &'static str {
    match scope {
        GenreBackfillScope::OctoDownloads => "OctoDownloads",
        GenreBackfillScope::WholeLibrary => "WholeLibrary",
    }
}

fn status_name(status: GenreBackfillStatus) -> &'static str {
    match status {
        GenreBackfillStatus::Idle => "Idle",
        GenreBackfillStatus::Running => "Running",
        GenreBackfillStatus::Completed => "Completed",
        GenreBackfillStatus::Cancelled => "Cancelled",
        GenreBackfillStatus::Interrupted => "Interrupted",
        GenreBackfillStatus::Failed => "Failed",
    }
}

/// `_config["Library:DownloadPath"] ?? "./downloads"`.
fn music_path(state: &AppState) -> String {
    state
        .settings
        .raw("Library:DownloadPath")
        .unwrap_or_else(|| "./downloads".to_string())
}

/// The genre rules changed after this run was planned. Apply re-plans from the current rules,
/// so a preview in this state no longer describes what Apply would write.
fn settings_changed(state: &AppState, settings_hash: Option<&str>) -> bool {
    settings_hash
        .is_some_and(|hash| hash != GenreBackfillWorker::hash_settings(&state.settings.current().genre))
}

/// `GenreBackfillStartRequest(string? Scope, bool DryRun, string? Confirm)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GenreBackfillStartRequest {
    scope: Option<String>,
    dry_run: bool,
    confirm: Option<String>,
}

/// Start a backfill, or a preview of one.
///
/// DryRun is the default path in the UI: the button says "Preview changes" and only the preview
/// screen offers to apply. A whole-library APPLY additionally requires the caller to type the
/// library path back, because that run rewrites files Octo never created and a drive-by POST
/// must not be able to start it.
async fn start_genre_backfill(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: GenreBackfillStartRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let scope = match enum_try_parse(&SCOPES, request.scope.as_deref()) {
        Some(1) => GenreBackfillScope::WholeLibrary,
        _ => GenreBackfillScope::OctoDownloads,
    };
    if !state.settings.current().genre.enabled {
        return browse.finish(error(
            StatusCode::BAD_REQUEST,
            "Turn genre normalization on first, or a run would change nothing.",
        ));
    }
    let root = music_path(&state);
    if scope == GenreBackfillScope::WholeLibrary
        && !request.dry_run
        && request.confirm.as_deref().map(str::trim) != Some(root.as_str())
    {
        return browse.finish(error(
            StatusCode::BAD_REQUEST,
            format!("To rewrite the whole library, type the music path exactly: {root}"),
        ));
    }
    if !state
        .genre_backfill_worker
        .try_enqueue(GenreBackfillRequest::new(scope, request.dry_run))
    {
        return browse.finish(error(StatusCode::CONFLICT, ALREADY_RUNNING));
    }
    info!(
        "Genre backfill requested: scope {}, dryRun {}",
        scope_name(scope),
        if request.dry_run { "True" } else { "False" }
    );
    browse.finish(status(
        StatusCode::ACCEPTED,
        &json!({ "started": true, "scope": scope_name(scope), "dryRun": request.dry_run }),
    ))
}

async fn get_genre_backfill(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let run = state.genre_backfill_worker.current();
    let preview: Vec<Value> = run
        .preview
        .iter()
        .map(|c| {
            json!({
                "path": c.path,
                "before": c.before,
                "after": c.after,
                "action": c.action,
                "rule": c.rule,
            })
        })
        .collect();
    browse.finish(ok(&json!({
        "runId": run.run_id,
        "status": status_name(run.status),
        "scope": scope_name(run.scope),
        "dryRun": run.dry_run,
        "startedUtc": utc_opt(&run.started_utc),
        "finishedUtc": utc_opt(&run.finished_utc),
        "total": run.total,
        "processed": run.processed,
        "changed": run.changed,
        "cleared": run.cleared,
        "skipped": run.skipped,
        "failed": run.failed,
        "lastPath": run.last_path,
        "reason": run.reason,
        "errors": run.errors,
        "preview": preview,
        "canResume": run.can_resume(),
        "canUndo": state.genre_backfill_journal.exists(),
        "settingsChanged": settings_changed(&state, run.settings_hash.as_deref()),
        "musicPath": music_path(&state),
    })))
}

async fn cancel_genre_backfill(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    state.genre_backfill_worker.request_cancel();
    browse.finish(status(StatusCode::ACCEPTED, &json!({ "cancelling": true })))
}

async fn resume_genre_backfill(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let run = state.genre_backfill_worker.current();
    if !run.can_resume() {
        return browse.finish(error(StatusCode::BAD_REQUEST, "There is nothing to resume."));
    }
    // A run's settings are snapshotted when it starts. Resuming under edited rules would finish
    // the library under a different table from the one it started with.
    if settings_changed(&state, run.settings_hash.as_deref()) {
        return browse.finish(error(
            StatusCode::CONFLICT,
            "The genre rules changed since this run started. Preview again instead of resuming.",
        ));
    }
    if !state
        .genre_backfill_worker
        .try_enqueue(GenreBackfillRequest::new(run.scope, run.dry_run))
    {
        return browse.finish(error(StatusCode::CONFLICT, ALREADY_RUNNING));
    }
    browse.finish(status(StatusCode::ACCEPTED, &json!({ "resumed": true })))
}

/// Put every genre frame the last backfill changed back the way it was.
///
/// The journal is the only real undo, and it covers the genre frame only. The tag writer
/// rewrites the whole tag block, so anything it does not round-trip was lost on the first save;
/// entries are keyed by path, so a moved file stays rewritten; and if the journal is gone there
/// is no undo at all. The dashboard says all three next to the button.
async fn undo_genre_backfill(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    if !state.genre_backfill_journal.exists() {
        return browse.finish(error(StatusCode::BAD_REQUEST, "There is no backfill to undo."));
    }
    if !state
        .genre_backfill_worker
        .try_enqueue(GenreBackfillRequest::undo())
    {
        return browse.finish(error(StatusCode::CONFLICT, ALREADY_RUNNING));
    }
    info!("Genre backfill undo requested");
    browse.finish(status(StatusCode::ACCEPTED, &json!({ "started": true })))
}

/// The broad-genre preset, served from the one place it is defined. A second copy in admin.js
/// would be a table the dashboard and the tests could disagree about.
///
/// An explicit projection with PascalCase keys, not the API's camelCase: this payload is posted
/// straight back to the settings API, which reads exact PascalCase and the match mode as its
/// NAME. Emitted any other way the shipped preset cannot be saved, which is how it shipped the
/// first time.
async fn get_genre_presets() -> Response {
    let broad: Vec<Value> = GenreSettings::broad_genre_preset()
        .into_iter()
        .map(|rule| {
            json!({
                "Id": rule.id,
                "Pattern": rule.pattern,
                "Genre": rule.genre,
                "Match": match rule.match_mode {
                    GenreMatchMode::Contains => "Contains",
                    GenreMatchMode::Exact => "Exact",
                },
                "Enabled": rule.enabled,
            })
        })
        .collect();
    ok(&json!({ "broad": broad }))
}
