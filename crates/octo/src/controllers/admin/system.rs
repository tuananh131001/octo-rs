//! `AdminController`'s process and status actions: the service health dots (`status`), the
//! library path chain (`library-status`), server discovery, Lidarr's choices and connection
//! test, the restart, the test notification, the fetched-songs log, the hearted downloads, and
//! the two recovery levers (clear rejected peers, clear the metadata caches).

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::Utc;
use octo_core::common::dotnet::is_blank;
use octo_core::settings::HeartDownloadSource;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use super::helpers_6b1::{bind_body, camel_case_value};
use crate::app::AppState;
use crate::http::error::{json_ok, json_status};
use crate::services::admin::directory_browser::is_writable;
use crate::services::http_client_factory::{connect_failure_message, timeout_message};
use crate::services::soulseek::soulseek_link::ISoulseekLink;
use crate::services::subsonic::subsonic_response_builder::acquisition_json;

/// `GET /api/admin/discover-servers`: scans the local network for Subsonic/Navidrome servers so
/// the setup UI can offer a detected upstream URL instead of requiring it typed by hand. Empty
/// when Octo cannot see the LAN (a Docker bridge network, for example).
pub async fn discover_servers(State(state): State<AppState>) -> Response {
    let servers = state.subsonic_discovery.scan().await;
    json_ok(&json!({ "servers": servers }))
}

/// `GET /api/admin/library-status`: where downloads will actually land, and why. Octo fronts
/// Navidrome, so the library Navidrome scans is the source of truth and the default; this
/// states the whole chain (what Navidrome reports, whether Octo can see it, what is therefore
/// in effect) so "my downloads went nowhere" is answerable from the UI instead of the logs.
pub async fn library_status(State(state): State<AppState>) -> Response {
    let subsonic = state.settings.current().subsonic.clone();
    let configured = state.settings.raw("Library:DownloadPath").unwrap_or_default();
    let identity = &state.navidrome_identity;

    // Cheap when already detected: this is cached inside the service.
    identity.detect_music_folder(false).await;

    let reported = identity.detected_music_folder();
    let effective = identity.effective_download_path(&configured);
    let is_dir = |path: &str| std::path::Path::new(path).is_dir();
    let libraries: Vec<serde_json::Value> = identity
        .known_libraries()
        .into_iter()
        .map(|library| {
            json!({
                "id": library.id,
                "name": library.name,
                "folder": library.folder,
                "visible": is_dir(&library.folder),
            })
        })
        .collect();

    json_ok(&json!({
        "autoDetect": subsonic.auto_detect_download_path,
        "pinnedLibraryPath": subsonic.library_path,
        "navidromeReports": reported,
        // Navidrome describes paths as IT sees them. Whether Octo can see the same path is the
        // difference between downloads being scanned and vanishing, so it is stated rather than
        // implied.
        "visibleToOcto": reported.as_deref().is_some_and(|r| !r.is_empty() && is_dir(r)),
        "configuredFallback": configured,
        "effectiveDownloadPath": effective,
        "writable": !effective.is_empty() && is_dir(&effective) && is_writable(&effective),
        "rescanAuthenticated": identity.get_scan_auth().is_some(),
        "libraries": libraries,
    }))
}

/// `POST /api/admin/test-notification`: sends a test notification through every sink so URLs
/// and tokens can be verified without waiting for a real download. Reports each sink's outcome,
/// including the transport's real error text on failure.
pub async fn test_notification(State(state): State<AppState>) -> Response {
    let results = state.notifications.send_test().await;
    json_ok(&json!({ "results": results }))
}

/// `GET /api/admin/downloads`: the running log of songs Octo has fetched, newest first.
pub async fn downloads(State(state): State<AppState>) -> Response {
    let recent = state.download_history.get_recent(200);
    // The entries are written to disk PascalCase; the API answer is camelCase, with the
    // report's dictionaries keeping their keys.
    let value = serde_json::to_value(&recent).unwrap_or_default();
    json_ok(&json!({ "downloads": camel_case_value(value, &["fields", "stageSeconds"]) }))
}

/// `GET /api/admin/acquisitions`: every hearted download in flight or ended in the last half
/// hour, everyone's, newest first. The rows the app reads through getAcquisitions, plus the
/// provider key, and who asked only while Record who asked is on, the rule the fetched-songs
/// log follows too.
pub async fn acquisitions(State(state): State<AppState>) -> Response {
    let show_askers = state.settings.current().subsonic.record_requested_by;
    let rows: Vec<serde_json::Value> = state
        .acquisition_tracker
        .all()
        .iter()
        .map(|row| {
            let mut fields = acquisition_json(row);
            fields.insert("provider".into(), json!(row.provider));
            fields.insert("externalId".into(), json!(row.external_id));
            if show_askers && !row.requested_by.is_empty() {
                fields.insert("requestedBy".into(), json!(row.requested_by));
            }
            serde_json::Value::Object(fields)
        })
        .collect();
    json_ok(&json!({ "acquisitions": rows }))
}

/// Health of one backing service. `configured` is false for an optional service nobody set
/// up, which the dashboard shows as off rather than as a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ServiceProbe {
    ok: bool,
    detail: String,
    warning: bool,
    configured: bool,
}

impl ServiceProbe {
    fn new(ok: bool, detail: impl Into<String>) -> Self {
        ServiceProbe {
            ok,
            detail: detail.into(),
            warning: false,
            configured: true,
        }
    }

    fn warning(mut self, warning: bool) -> Self {
        self.warning = warning;
        self
    }

    fn unconfigured(mut self) -> Self {
        self.configured = false;
        self
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The message an `HttpClient` failure carried in C#, for a probe's detail.
fn probe_failure(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        timeout_message(PROBE_TIMEOUT)
    } else {
        connect_failure_message(error)
    }
}

/// A GET with the default client and a 5 second timeout: the status, or what went wrong.
async fn probe_get(state: &AppState, url: &str) -> Result<reqwest::StatusCode, String> {
    let request = state
        .http
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| format!("An invalid request URI was provided. {e}"))?;
    state
        .http
        .execute(request)
        .await
        .map(|response| response.status())
        .map_err(|e| probe_failure(&e))
}

async fn probe_navidrome(state: &AppState) -> ServiceProbe {
    let url = state.settings.current().subsonic.url.clone().unwrap_or_default();
    if is_blank(&url) {
        return ServiceProbe::new(false, "Not set up yet. Enter your server under Music server.");
    }
    // Navidrome's /rest/ping requires auth, but it returns 200 with an error body even on bad
    // credentials, which proves connectivity.
    let target = format!(
        "{}/rest/ping?u=probe&p=probe&v=1.16.1&c=octo&f=json",
        url.trim_end_matches('/')
    );
    match probe_get(state, &target).await {
        Ok(status) => ServiceProbe::new(
            status.is_success(),
            format!("HTTP {} from {url}", status.as_u16()),
        ),
        Err(message) => ServiceProbe::new(false, message),
    }
}

async fn probe_slskd(state: &AppState) -> ServiceProbe {
    // slskd answering is not slskd able to search: it can be up and out of Soulseek.
    let reading = state.soulseek_link.read(true).await;
    let hold = state.settings.current().soulseek.effective_outage_hold_hours();
    let (up, warning, detail) = octo_core::soulseek::soulseek_link::describe(reading.as_ref(), hold);
    ServiceProbe::new(up, detail).warning(warning)
}

async fn probe_lidarr(state: &AppState) -> ServiceProbe {
    let settings = state.settings.current();
    let lidarr = &settings.lidarr;
    let lidarr_enabled = settings
        .subsonic
        .effective_heart_download_sources()
        .iter()
        .any(|step| {
            step.source == HeartDownloadSource::Lidarr
                && (step.song_enabled == Some(true) || step.album_enabled == Some(true))
        });
    // An absent optional service is a calm state, not a warning: most installs never configure
    // Lidarr and their dashboard should not carry a permanent yellow dot for it. Yellow means
    // "you enabled it but haven't finished setting it up": incomplete config, as opposed to an
    // outage.
    if lidarr.base_url.as_deref().is_none_or(is_blank) || lidarr.api_key.as_deref().is_none_or(is_blank) {
        return if lidarr_enabled {
            ServiceProbe::new(true, "selected but not configured").warning(true)
        } else {
            ServiceProbe::new(true, "Not set up. Optional.").unconfigured()
        };
    }
    if lidarr_enabled
        && (lidarr.root_folder_path.as_deref().is_none_or(is_blank)
            || lidarr.quality_profile_id <= 0
            || lidarr.metadata_profile_id <= 0)
    {
        return ServiceProbe::new(true, "select a root folder and profiles").warning(true);
    }
    let ok = state.lidarr_client.is_reachable().await;
    ServiceProbe::new(
        ok,
        if ok {
            "reachable"
        } else {
            "unreachable / API key invalid"
        },
    )
}

async fn probe_you_tube_shim(state: &AppState) -> ServiceProbe {
    let shim = state
        .settings
        .raw("YouTube:ShimUrl")
        .unwrap_or_else(|| "http://yt-dlp-shim:8080".to_string());
    let target = format!("{}/health", shim.trim_end_matches('/'));
    match probe_get(state, &target).await {
        Ok(status) => ServiceProbe::new(status.is_success(), format!("HTTP {}", status.as_u16())),
        Err(message) => ServiceProbe::new(false, message),
    }
}

async fn probe_last_fm(state: &AppState) -> ServiceProbe {
    let key = state.settings.current().last_fm.api_key.clone();
    // Optional: without a key search still shows your library and Deezer albums, and radio
    // falls back to Navidrome. Red here read as a broken install.
    if key.is_empty() {
        return ServiceProbe::new(true, "No API key, so song discovery and radio are off. Optional.")
            .unconfigured();
    }
    let target = format!(
        "https://ws.audioscrobbler.com/2.0/?method=track.getInfo&artist=cher&track=believe&api_key={key}&format=json"
    );
    match probe_get(state, &target).await {
        Ok(status) => ServiceProbe::new(status.is_success(), format!("HTTP {}", status.as_u16())),
        Err(message) => ServiceProbe::new(false, message),
    }
}

/// `DateTimeOffset.UtcNow.ToString("O")`: seven fraction digits and `+00:00`.
fn round_trip_offset(now: chrono::DateTime<Utc>) -> String {
    format!(
        "{}.{:07}+00:00",
        now.format("%Y-%m-%dT%H:%M:%S"),
        now.timestamp_subsec_nanos() / 100
    )
}

/// `GET /api/admin/status`: a quick health snapshot of each backing service, so the dashboard
/// shows at a glance whether Octo can reach Navidrome, slskd, Lidarr, the yt-dlp shim and
/// Last.fm. The probes run in parallel: the total is the slowest, not the sum.
pub async fn get_status(State(state): State<AppState>) -> Response {
    let (navidrome, slskd, lidarr, shim, lastfm) = tokio::join!(
        probe_navidrome(&state),
        probe_slskd(&state),
        probe_lidarr(&state),
        probe_you_tube_shim(&state),
        probe_last_fm(&state),
    );
    json_ok(&json!({
        "octo": ServiceProbe::new(true, "Octo is responding"),
        "services": {
            "navidrome": navidrome,
            "slskd": slskd,
            "lidarr": lidarr,
            "ytDlpShim": shim,
            "lastfm": lastfm,
        },
        "time": round_trip_offset(Utc::now()),
    }))
}

/// `GET /api/admin/lidarr/options`: the choices the connected Lidarr offers for add-album
/// defaults.
pub async fn get_lidarr_options(State(state): State<AppState>) -> Response {
    match state.lidarr_client.get_options().await {
        Ok(options) => json_ok(&options),
        Err(e) => json_status(StatusCode::BAD_REQUEST, &json!({ "error": e.to_string() })),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LidarrConnectionTestRequest {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// `POST /api/admin/lidarr/test`: tests entered credentials without saving them.
pub async fn test_lidarr_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: LidarrConnectionTestRequest = match bind_body(&headers, &body, "request") {
        Ok(request) => request,
        Err(res) => return res,
    };
    match state
        .lidarr_client
        .test_connection(
            request.base_url.as_deref().unwrap_or(""),
            request.api_key.as_deref().unwrap_or(""),
        )
        .await
    {
        Ok(options) => json_ok(&json!({
            "ok": true,
            "message": "Connected to Lidarr. Choices loaded.",
            "options": options,
        })),
        Err(e) => json_status(
            StatusCode::BAD_REQUEST,
            &json!({ "ok": false, "error": e.to_string() }),
        ),
    }
}

/// `POST /api/admin/restart`: exits the process so docker compose's restart policy brings the
/// container back up with refreshed config. The caller gets its 202 before the shutdown fires.
pub async fn restart(State(state): State<AppState>) -> Response {
    warn!("Admin requested restart; container will exit in 1s");
    // Fire-and-forget so the response can be returned first.
    let inner = state.inner.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        inner.stop_application();
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Belt and braces: if the graceful stop has not finished in 2s, exit hard so
        // docker-compose treats it as a crash and restarts.
        std::process::exit(1);
    });
    json_status(
        StatusCode::ACCEPTED,
        &json!({ "ok": true, "message": "restarting" }),
    )
}

/// `POST /api/admin/soulseek/rejected-peers/clear`: forgets every peer and file that download
/// verification rejected. The recovery lever for a wrong denial: entries lapse on their own
/// after 30 days, but a user watching a track stop being fetchable should not have to wait a
/// month to find out whether this list is why.
pub async fn clear_rejected_peers(State(state): State<AppState>) -> Response {
    let cleared = state.rejected_peers.clear();
    info!("Rejected-peer memory cleared by admin request ({cleared} entries)");
    json_ok(&json!({ "cleared": cleared }))
}

/// `POST /api/admin/clear-metadata-cache`: drops every cached metadata answer and cover image.
/// Cached entries expire on their own, so this is a recovery lever rather than routine
/// maintenance: it turns "wait for the TTL" into "fixed now" when a run of throttled upstream
/// calls has left albums or covers looking wrong.
pub async fn clear_metadata_cache(State(state): State<AppState>) -> Response {
    state.deezer_metadata.clear_caches();
    state.cover_art_aggregator.clear_cache();
    info!("Metadata and cover-art caches cleared by admin request");
    json_ok(&json!({ "cleared": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn status_time_is_written_as_round_trip_with_an_offset() {
        let now = Utc.with_ymd_and_hms(2026, 10, 4, 8, 7, 57).unwrap()
            + chrono::TimeDelta::nanoseconds(635_173_300);
        assert_eq!(round_trip_offset(now), "2026-10-04T08:07:57.6351733+00:00");
    }
}
