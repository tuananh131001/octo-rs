//! Port of `Controllers/UpdateController.cs`: the dashboard's update card. Whether a newer
//! release is out, and Update now, which hands the update to the host helper (scripts/updater).
//! Without the helper the card shows the command.
//!
//! Behind the same guard as the rest of /api/admin: writes need X-Octo-Admin. Anyone who can use
//! the dashboard can already restart Octo; this only adds moving it to the newest published
//! release of the configured repo, which the helper checks again on its side.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use octo_core::json::datetime::min_value;
use serde::Deserialize;
use serde_json::{Value, json};

use super::helpers_6b2::{BROWSE_COOKIE_NAME, bind_body, cookie, error, ok, split, status, utc_opt};
use crate::app::AppState;
use crate::http::error::AppError;
use crate::http::routes::RouteSet;
use crate::services::updates::release_check::{ReleaseCheckView, ReleaseNote};
use crate::services::updates::update_host::{
    UpdateHost, UpdateRequestError, UpdateRunStates, UpdateRunStatus,
};

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/update", get(get_update).post(update))
        .route("/api/admin/update/check", post(check))
}

/// `UpdateRequest(string? Tag)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UpdateRequest {
    tag: Option<String>,
}

async fn get_update(State(state): State<AppState>) -> Response {
    ok(&view(&state, state.release_check.view()))
}

/// Ask GitHub now. At most once a minute; sooner returns the last answer.
async fn check(State(state): State<AppState>) -> Response {
    let answer = state.release_check.check(true).await;
    ok(&view(&state, answer))
}

/// Update to the newest release, through the host helper.
async fn update(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: UpdateRequest =
        match bind_body(&parts, &body, "Octo.Controllers.UpdateController+UpdateRequest") {
            Ok(r) => r,
            Err(answer) => return *answer,
        };
    let view = state.release_check.view();
    if !view.enabled {
        return error(
            StatusCode::CONFLICT,
            "Update checks are off, so Octo does not know which release is newest.",
        );
    }
    let latest = match &view.latest {
        Some(latest) if view.update_available => latest.tag.clone(),
        _ => return error(StatusCode::CONFLICT, "There is no newer release to update to."),
    };
    if request.tag.as_deref() != Some(latest.as_str()) {
        return error(
            StatusCode::CONFLICT,
            format!("Only the newest release, {latest}, can be installed from here."),
        );
    }
    let host = &state.update_host;
    if host.helper().is_none() {
        return error(
            StatusCode::CONFLICT,
            "The update helper is not installed on this server's host. Run the command shown instead.",
        );
    }
    if host.busy() {
        return error(StatusCode::CONFLICT, "An update is already under way.");
    }

    let user = state
        .browse_sessions
        .user_of(cookie(&parts.headers, BROWSE_COOKIE_NAME).as_deref())
        .unwrap_or_else(|| "dashboard".to_string());
    match host.request(&latest, &user) {
        Ok(id) => status(
            StatusCode::ACCEPTED,
            &json!({ "ok": true, "id": id, "tag": latest }),
        ),
        // ArgumentException → 400 "Invalid request"; an I/O failure → 500.
        Err(UpdateRequestError::NotARelease(tag)) => AppError::Argument(tag).into_response(),
        Err(UpdateRequestError::Io(e)) => AppError::Internal(e.into()).into_response(),
    }
}

/// A release as the controller wrote it (camelCase).
fn note_json(note: &ReleaseNote) -> Value {
    json!({
        "tag": note.tag,
        "name": note.name,
        "notes": note.notes,
        "url": note.url,
        "publishedUtc": utc_opt(&note.published_utc),
    })
}

fn view(state: &AppState, view: ReleaseCheckView) -> Value {
    let host = &state.update_host;
    let helper = host.helper();
    let pending = host.pending();
    let run = host.status().map(|status| describe(host, status, &view.running));
    let tag = view
        .latest
        .as_ref()
        .map_or("<release>".to_string(), |l| l.tag.clone());
    let helper = match helper {
        None => json!({ "installed": false }),
        Some(h) => json!({
            "installed": true,
            "version": h.version,
            "mode": h.mode,
            "dir": h.dir,
            "installedUtc": utc_opt(&h.installed_utc),
        }),
    };
    json!({
        "enabled": view.enabled,
        "repo": view.repo,
        "running": view.running,
        "latest": view.latest.as_ref().map(note_json),
        "newer": view.newer.iter().map(note_json).collect::<Vec<_>>(),
        "updateAvailable": view.update_available,
        "standing": view.standing,
        "checkedUtc": utc_opt(&view.checked_utc),
        "error": view.error,
        "helper": helper,
        "pending": pending.is_some(),
        "pendingId": pending,
        "unanswered": host.unanswered(),
        "run": run,
        // What to run by hand, from the Octo folder: the built-from-source install, and the image one.
        "command": format!(
            "git fetch --tags && git checkout --detach {tag} && docker compose build && docker compose up -d"
        ),
        "imageCommand": "docker compose pull octo && docker compose up -d octo",
    })
}

fn describe(host: &UpdateHost, status: UpdateRunStatus, running: &str) -> Value {
    let mut state = status.state.clone();
    // The helper's last word before Octo restarted was "restarting"; Octo being back on that
    // release is the proof it finished.
    if state == UpdateRunStates::RESTARTING && status.tag.as_deref() == Some(running) {
        state = UpdateRunStates::DONE.to_string();
    }
    let mut error = status.error.clone();
    if UpdateRunStates::running(&state)
        && Utc::now() - status.started_utc.unwrap_or_else(min_value) > UpdateHost::RUN_TIMEOUT
    {
        state = UpdateRunStates::FAILED.to_string();
        error.get_or_insert_with(|| {
            "The update helper stopped reporting. Its log on the host says why.".to_string()
        });
    }
    let log = if state == UpdateRunStates::FAILED {
        host.log_tail(40)
    } else {
        Vec::new()
    };
    json!({
        "id": status.id,
        "tag": status.tag,
        "from": status.from,
        "state": state,
        "step": status.step,
        "error": error,
        "startedUtc": utc_opt(&status.started_utc),
        "finishedUtc": utc_opt(&status.finished_utc),
        "log": log,
    })
}
