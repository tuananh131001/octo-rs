//! Octo's own Subsonic extensions and the extension list (`SubsonicController.GetAcquisitions`
//! L2540, `GetOpenSubsonicExtensions` L2568, `GetLibraryActions` L2587, `GetUpgrades` L2602,
//! `ApplyLibraryAction` L2629, `JukeboxControl` L3237; endpoints.md §3.1, §3.8).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::common::dotnet::{self, is_blank};
use octo_core::settings::LibraryAction;
use octo_subsonic::subsonic_response_builder::{REMOVE_ACTION, UPGRADE_ACTION};
use tracing::info;

use super::helpers_6a2::{SubsonicCall, check_caller, is_successful_subsonic_response, signed_in_user};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::library::{LibraryActionOutcome, LibraryActionRequest, LibraryActionState, UpgradeAsk};
use crate::services::subsonic::SubsonicResponseBuilderExt;

/// `UpgradeStates.Working`.
const UPGRADE_WORKING: &str = "working";

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("getAcquisitions", get(get_acquisitions).post(get_acquisitions))
        .subsonic(
            "getOpenSubsonicExtensions",
            get(get_open_subsonic_extensions).post(get_open_subsonic_extensions),
        )
        .subsonic(
            "getLibraryActions",
            get(get_library_actions).post(get_library_actions),
        )
        .subsonic("getUpgrades", get(get_upgrades).post(get_upgrades))
        .subsonic(
            "libraryAction",
            get(apply_library_action).post(apply_library_action),
        )
        .subsonic("jukeboxControl", get(jukebox_control).post(jukebox_control))
}

/// The caller's own hearted downloads, while they run and for half an hour after, so the
/// app can draw progress on the button that started one. Octo answers this itself, so it
/// works wherever the Subsonic API does, including away from home, unlike the admin API.
///
/// Credentials are checked the way a station or a mix checks them: a ping to Navidrome with
/// the caller's own. Only rows that user asked for come back, admin or not.
pub async fn get_acquisitions(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;

    let mut auth = call.parameters.clone();
    auth.insert("f".into(), FORMAT.into());
    let Some(check) = call.proxy.relay_safe("rest/ping", &auth).await else {
        return Ok(builder
            .create_error(FORMAT, 0, "Octo can't reach Navidrome to check who is asking")
            .into_response());
    };
    if !is_successful_subsonic_response(&check.body, FORMAT) {
        return Ok(builder
            .create_error(FORMAT, 40, "Wrong username or password")
            .into_response());
    }

    let username = signed_in_user(&state, &call).await?;
    let rows = if is_blank(&username) {
        Vec::new()
    } else {
        state.acquisition_tracker.for_user(&username)
    };
    Ok(builder.create_acquisitions_response(&rows).into_response())
}

/// Navidrome's extension list with octoAcquisitions added, so a client can tell this server
/// answers getAcquisitions before it asks, and octoLibraryActions while library actions are
/// on. Relayed, then merged; no credentials are needed, as the OpenSubsonic spec has it.
pub async fn get_open_subsonic_extensions(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format();
    let relay = call
        .proxy
        .relay_safe("rest/getOpenSubsonicExtensions", &call.parameters)
        .await;
    let settings = state.settings.current();
    state
        .subsonic_response_builder
        .merge_open_subsonic_extensions(
            &format,
            relay.as_ref().map(|r| r.body.as_ref()),
            relay.as_ref().and_then(|r| r.content_type.as_deref()),
            settings.metadata.fetch_lyrics,
            settings.library_actions.enabled,
        )
        .into_response()
}

/// Whether a source Better quality searches (Soulseek, Lidarr) is set up here at all.
fn upgrade_ready(state: &AppState) -> bool {
    state.upgrade_sources.ready()
}

/// Where Better quality looks, in words: "Soulseek", "Lidarr", or "Soulseek or Lidarr".
fn upgrade_source_name(state: &AppState) -> String {
    state.upgrade_sources.name()
}

/// octoLibraryActions v1: what the caller may do to library files from the app, read from the
/// settings as they are now. Always JSON. Credentials are checked with a ping to Navidrome, as
/// getAcquisitions checks them.
pub async fn get_library_actions(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    state
        .subsonic_response_builder
        .create_library_actions_response(
            &state.settings.current().library_actions,
            call.param_opt("u"),
            state.download_concurrency.current(),
            upgrade_ready(&state),
            &upgrade_source_name(&state),
        )
        .into_response()
}

/// octoLibraryActions v2: the caller's upgrade jobs and how each is going, read by the apps while
/// an upgrade they asked for is running. Always JSON; credentials checked like getLibraryActions.
pub async fn get_upgrades(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    let jobs = match call.param_opt("u") {
        Some(username) if !is_blank(username) => state.upgrade_queue.snapshot_for(Some(username)),
        _ => Vec::new(),
    };
    let mut progress: HashMap<String, Option<f64>> = HashMap::new();
    for row in state.acquisition_tracker.all() {
        progress
            .entry(format!("{}:{}", row.provider, row.external_id))
            .or_insert(row.progress);
    }
    let rows: Vec<_> = jobs
        .into_iter()
        .map(|job| {
            let live = match &job.acquisition_key {
                Some(key) if job.state == UPGRADE_WORKING => progress.get(key).copied().flatten(),
                _ => None,
            };
            (job, live)
        })
        .collect();
    state
        .subsonic_response_builder
        .create_upgrades_response(&rows)
        .into_response()
}

/// octoLibraryActions v1: remove one song, exactly as putting it in the Delete action playlist
/// does. The executor applies every gate: the master switch, the allowlist, Delete being on,
/// the dry run, and a file it can prove is this song, which goes to quarantine.
///
/// Never through a rating: where rating actions are on, one star can remove a song, and this
/// is how an app removes one without giving it a rating.
pub async fn apply_library_action(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }

    let id = call.param("id").trim().to_string();
    let action = call.param("action").trim().to_string();
    if id.is_empty() || action.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: id and action")
            .into_response());
    }
    if dotnet::eq_ignore_case(&action, UPGRADE_ACTION) {
        return Ok(queue_upgrade(&state, &id, call.param_opt("u")));
    }
    if !dotnet::eq_ignore_case(&action, REMOVE_ACTION) {
        return Ok(builder
            .create_error(
                FORMAT,
                0,
                &format!("Unknown action \"{action}\"; this server knows remove and upgrade"),
            )
            .into_response());
    }

    // The name the ping just checked. An API key alone names nobody here, so it cannot be on
    // the allowlist, and the executor is not asked at all.
    let Some(username) = call.param_opt("u").filter(|u| !is_blank(u)).map(str::to_string) else {
        return Ok(builder
            .create_library_action_outcome_response(
                &id,
                &LibraryActionOutcome::new(
                    LibraryActionState::Skipped,
                    Some(
                        "Sign in with a username to remove songs; an API key alone does not say who is asking."
                            .into(),
                    ),
                ),
                REMOVE_ACTION,
            )
            .into_response());
    };

    // Not tied to the request: a client that hangs up must not stop a move halfway.
    let executor = Arc::clone(&state.library_action_executor);
    let request = LibraryActionRequest::new(LibraryAction::Delete, id.clone(), username.clone());
    let outcome = tokio::spawn(async move { executor.apply(request).await })
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
        .map_err(AppError::Internal)?;
    info!(
        "Library action Delete for {id} by {username} from the app: {} - {}",
        outcome.state.name(),
        outcome.detail.as_deref().unwrap_or("")
    );
    Ok(builder
        .create_library_action_outcome_response(&id, &outcome, REMOVE_ACTION)
        .into_response())
}

/// octoLibraryActions v2: queue a higher quality copy of one song, and answer at once. The same
/// gates as the Better quality playlist, read now so the app hears a reason straight away; the
/// executor checks them all again when the job runs. A job is listed in getUpgrades before this
/// answers "queued".
fn queue_upgrade(state: &AppState, id: &str, username: Option<&str>) -> Response {
    let builder = &state.subsonic_response_builder;
    let settings = state.settings.current().library_actions.clone();
    let username = username.filter(|u| !is_blank(u));
    let refusal = match username {
        None => Some(
            "Sign in with a username to upgrade songs; an API key alone does not say who is asking."
                .to_string(),
        ),
        Some(_) if !settings.enabled => Some("Library actions are off.".to_string()),
        Some(user) if !settings.is_allowed(Some(user)) => {
            Some(format!("{user} is not on the library actions allowed list."))
        }
        Some(_)
            if !settings
                .effective_actions()
                .iter()
                .any(|a| a.action == LibraryAction::BetterQuality && a.enabled) =>
        {
            Some("Better quality is not switched on.".to_string())
        }
        Some(_) if settings.dry_run => {
            Some("Library actions only rehearse while dry run is on, so nothing would change.".to_string())
        }
        Some(_) if !upgrade_ready(state) => Some(format!(
            "Better quality looks for copies on {}, which is not set up on this server.",
            upgrade_source_name(state)
        )),
        Some(_) => None,
    };
    if let Some(refusal) = refusal {
        return builder
            .create_library_action_response(id, "skipped", Some(&refusal), UPGRADE_ACTION)
            .into_response();
    }
    let username = username.unwrap_or_default();

    let (jobs, full) = state.upgrade_queue.add(
        vec![UpgradeAsk {
            navidrome_id: id.to_string(),
            ..Default::default()
        }],
        username,
        "app",
    );
    let Some(job) = jobs.first() else {
        return builder
            .create_library_action_response(id, "skipped", full.as_deref(), UPGRADE_ACTION)
            .into_response();
    };
    info!(
        "Higher quality for {id} asked by {username} from the app: {}",
        job.state
    );
    builder
        .create_library_action_response(
            id,
            "queued",
            Some(&format!(
                "Looking for a higher quality copy on {}.",
                upgrade_source_name(state)
            )),
            UPGRADE_ACTION,
        )
        .into_response()
}

/// Octo has no jukebox device. Relaying surfaced a misleading "Error connecting to Subsonic
/// server"; return a clean, plain "not supported" so the client just disables jukebox mode
/// instead of logging a scary error.
pub async fn jukebox_control(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    state
        .subsonic_response_builder
        .create_error(&call.format(), 0, "Jukebox is not supported")
        .into_response()
}
