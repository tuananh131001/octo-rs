//! Port of `Services/Subsonic/NavidromeIdentityService.cs`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use indexmap::IndexMap;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

/// One library as Navidrome reports it. `folder` is the path Octo should use: remotePath when
/// Navidrome supplies one (how other services see the same library), otherwise its own path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavidromeLibrary {
    pub id: Option<String>,
    pub name: Option<String>,
    pub folder: String,
}

const DETECT_TTL: Duration = Duration::from_secs(30 * 60);

/// Login tokens remembered for naming native requests.
const NATIVE_USERS: usize = 100;

/// Octo's own standing identity toward the upstream Navidrome. Octo is a proxy, so most
/// requests carry the client's credentials. But background work has no client in flight:
/// detecting where Navidrome keeps its music, and triggering a rescan after a download. This
/// service supplies that identity two ways, in priority order:
///   1. Configured admin creds (Subsonic:AdminUsername / AdminPassword), if set.
///   2. Captured from traffic: when a client signs in through the relayed native POST
///      /auth/login, we cache the returned admin JWT + subsonic salt/token.
///
/// From that identity it auto-detects the music folder from Navidrome's native GET
/// /api/library, so downloads land where Navidrome actually scans no matter how the two are
/// configured, instead of relying on a hand-kept DownloadPath that can silently drift from the
/// server's real music directory.
///
/// A cheap handle: clones share one identity.
#[derive(Clone)]
pub struct NavidromeIdentityService {
    inner: Arc<Inner>,
}

struct Inner {
    // Read at the point of use (IOptionsMonitor, not IOptions): the admin UI writes
    // settings.json and the config provider reloads it, so a captured copy would serve startup
    // values until a restart while the admin UI SHOWED the new value.
    settings: Arc<SettingsStore>,
    http: reqwest::Client,
    state: Mutex<State>,
    /// `_detectGate`: one detection at a time.
    detect_gate: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    jwt: Option<String>,
    subsonic_token: Option<String>,
    subsonic_salt: Option<String>,
    username: Option<String>,
    native_users: IndexMap<String, String>,
    detected_folder: Option<String>,
    libraries: Vec<NavidromeLibrary>,
    detected_at: Option<Instant>,
}

impl State {
    fn fresh_folder(&self) -> Option<String> {
        let folder = self.detected_folder.as_ref().filter(|f| !f.is_empty())?;
        self.detected_at
            .is_some_and(|at| at.elapsed() < DETECT_TTL)
            .then(|| folder.clone())
    }
}

/// `JsonElement.GetString()`: a string, `None` for JSON null, and the exception it threw for
/// any other kind.
fn get_string(value: &Value) -> Result<Option<String>, String> {
    match value {
        Value::String(s) => Ok(Some(s.clone())),
        Value::Null => Ok(None),
        other => Err(format!(
            "The requested operation requires an element of type 'String', but the target element has type '{}'.",
            kind_name(other)
        )),
    }
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

/// `JsonElement.ToString()`: a string's value, any other value's JSON text.
fn element_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl NavidromeIdentityService {
    pub fn new(settings: Arc<SettingsStore>, http: reqwest::Client) -> Self {
        NavidromeIdentityService {
            inner: Arc::new(Inner {
                settings,
                http,
                state: Mutex::new(State::default()),
                detect_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The Navidrome music folder detected from /api/library, or `None`.
    pub fn detected_music_folder(&self) -> Option<String> {
        self.inner.state.lock().detected_folder.clone()
    }

    /// Every library Navidrome reported on the last successful detection. Empty until one has
    /// run. Used by the admin UI to offer a choice instead of silently adopting whichever one
    /// Navidrome listed first.
    pub fn known_libraries(&self) -> Vec<NavidromeLibrary> {
        self.inner.state.lock().libraries.clone()
    }

    /// Effective download path: an explicit override wins, else the Navidrome-detected music
    /// folder, else the caller's configured fallback. Auto-detect can be turned off, in which
    /// case the configured value is always used.
    pub fn effective_download_path(&self, configured_fallback: &str) -> String {
        if !self.inner.settings.current().subsonic.auto_detect_download_path {
            return configured_fallback.to_string();
        }
        match self.detected_music_folder() {
            Some(d) if !d.is_empty() => d,
            _ => configured_fallback.to_string(),
        }
    }

    /// Subsonic auth triplet (u/t/s) for authenticated Subsonic calls such as startScan, or
    /// `None` if no admin identity has been captured/configured yet.
    pub fn get_scan_auth(&self) -> Option<(String, String, String)> {
        let state = self.inner.state.lock();
        match (&state.username, &state.subsonic_token, &state.subsonic_salt) {
            (Some(u), Some(t), Some(s)) if !u.is_empty() && !t.is_empty() && !s.is_empty() => {
                Some((u.clone(), t.clone(), s.clone()))
            }
            _ => None,
        }
    }

    /// Cache the identity from a native login response body. Only admin logins are kept:
    /// /api/library and startScan need admin, and a later non-admin sign-in must not overwrite
    /// a good admin identity. Every login's token is remembered against its username, for
    /// naming the native requests that carry it.
    pub fn capture_login(&self, body: &[u8]) {
        // Not a login response we understand: ignore.
        let _ = self.try_capture_login(body);
    }

    fn try_capture_login(&self, body: &[u8]) -> Result<(), String> {
        let root: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let Value::Object(root) = root else {
            return Ok(());
        };
        let token = root.get("token").map(get_string).transpose()?.flatten();
        let is_admin = root.get("isAdmin") == Some(&Value::Bool(true));
        let login_username = root.get("username").map(get_string).transpose()?.flatten();
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Ok(());
        };

        if let Some(login_username) = login_username.filter(|u| !u.is_empty()) {
            let mut state = self.inner.state.lock();
            state.native_users.insert(token.clone(), login_username);
            while state.native_users.len() > NATIVE_USERS {
                state.native_users.shift_remove_index(0);
            }
        }
        if !is_admin {
            return Ok(());
        }

        let subsonic_token = root.get("subsonicToken").map(get_string).transpose()?;
        let subsonic_salt = root.get("subsonicSalt").map(get_string).transpose()?;
        let username = root.get("username").map(get_string).transpose()?;
        let user = {
            let mut state = self.inner.state.lock();
            state.jwt = Some(token);
            if let Some(value) = subsonic_token {
                state.subsonic_token = value;
            }
            if let Some(value) = subsonic_salt {
                state.subsonic_salt = value;
            }
            if let Some(value) = username {
                state.username = value;
            }
            state.username.clone()
        };
        info!(
            "Captured Navidrome admin identity from login (user={})",
            user.as_deref().unwrap_or("(null)")
        );
        // Refresh the detected music folder in the background off the new token.
        let service = self.clone();
        tokio::spawn(async move {
            service.detect_music_folder(true).await;
        });
        Ok(())
    }

    /// The user associated with a native login token captured while proxying /auth/login.
    /// Tokens and mappings are memory-only.
    pub fn username_for_native_token(&self, token: Option<&str>) -> Option<String> {
        let token = token.filter(|t| !t.trim().is_empty())?;
        self.inner.state.lock().native_users.get(token).cloned()
    }

    /// Detects Navidrome's music folder via GET /api/library. Prefers remotePath (the path as
    /// other services see the same library) then falls back to path. Cached with a TTL; pass
    /// `force` to bypass the cache.
    pub async fn detect_music_folder(&self, force: bool) -> Option<String> {
        if !force && let Some(folder) = self.inner.state.lock().fresh_folder() {
            return Some(folder);
        }
        let settings = self.inner.settings.current();
        if settings.subsonic.url.as_deref().is_none_or(str::is_empty)
            || !settings.subsonic.auto_detect_download_path
        {
            return None;
        }

        let _gate = self.inner.detect_gate.lock().await;
        if !force && let Some(folder) = self.inner.state.lock().fresh_folder() {
            return Some(folder);
        }
        match self.detect_locked().await {
            Ok(folder) => folder,
            Err(message) => {
                warn!("Navidrome music-folder detection failed: {message}");
                None
            }
        }
    }

    async fn detect_locked(&self) -> Result<Option<String>, String> {
        let Some(jwt) = self.ensure_jwt().await.filter(|j| !j.is_empty()) else {
            return Ok(None);
        };
        let settings = self.inner.settings.current();
        let url = format!(
            "{}/api/library",
            settings
                .subsonic
                .url
                .as_deref()
                .unwrap_or("")
                .trim_end_matches('/')
        );
        let response = self
            .inner
            .http
            .get(&url)
            .header("X-Nd-Authorization", format!("Bearer {jwt}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() == StatusCode::UNAUTHORIZED {
            // Captured token expired; force a fresh login next time.
            self.inner.state.lock().jwt = None;
            return Ok(None);
        }
        if !response.status().is_success() {
            return Ok(None);
        }
        let json = response.bytes().await.map_err(|e| e.to_string())?;
        let document: Value = serde_json::from_slice(&json).map_err(|e| e.to_string())?;
        let Value::Array(entries) = document else {
            return Ok(None);
        };
        if entries.is_empty() {
            return Ok(None);
        }

        // Navidrome can serve several libraries. Read them all: taking [0] and calling it "the"
        // music folder meant a multi-library server had its download target chosen by whatever
        // order Navidrome happened to return, with no way to say otherwise.
        let mut libraries = Vec::new();
        for entry in &entries {
            // remotePath is the path as OTHER services see this library, which is what Octo
            // needs; path is how Navidrome itself sees it.
            let remote = entry.get("remotePath").map(get_string).transpose()?.flatten();
            let path = entry.get("path").map(get_string).transpose()?.flatten();
            let Some(folder) = remote
                .filter(|r| !r.is_empty())
                .or(path)
                .filter(|f| !f.is_empty())
            else {
                continue;
            };
            libraries.push(NavidromeLibrary {
                id: entry.get("id").map(element_to_string),
                name: entry.get("name").map(get_string).transpose()?.flatten(),
                folder,
            });
        }
        if libraries.is_empty() {
            return Ok(None);
        }
        self.inner.state.lock().libraries = libraries.clone();

        // An explicit pin wins. Absent one, behave exactly as before and take the first entry,
        // so an existing install's download target cannot move on update. A pin that no longer
        // matches anything Navidrome reports falls back the same way rather than stranding
        // downloads.
        let pinned = settings.subsonic.library_path.trim();
        let mut chosen = &libraries[0];
        if !pinned.is_empty() {
            match libraries.iter().find(|l| l.folder == pinned) {
                Some(found) => chosen = found,
                None => warn!(
                    "Pinned library path '{pinned}' is not among the {} libraries Navidrome reports; using '{}' instead.",
                    libraries.len(),
                    chosen.folder
                ),
            }
        }
        let folder = chosen.folder.clone();

        // Safety gate: only ADOPT the detected path if it actually exists inside Octo's own
        // container. Navidrome reports the path as IT sees it, which is only useful to Octo when
        // the same directory is mounted at the same path here. If it isn't (different mount, or
        // Octo can't see it), keep the configured DownloadPath instead of silently redirecting
        // downloads to a folder Navidrome can't read. This is what makes auto-detect safe to
        // have on by default, including for existing installs on update.
        if !Path::new(&folder).is_dir() {
            warn!(
                "Navidrome reports its music folder as '{folder}', but that path is not mounted in Octo's container — keeping the configured download path. Mount '{folder}' into Octo (or set Navidrome's remotePath) to enable auto-detect."
            );
            return Ok(None);
        }

        {
            let mut state = self.inner.state.lock();
            state.detected_folder = Some(folder.clone());
            state.detected_at = Some(Instant::now());
        }
        info!("Detected Navidrome music folder: {folder}");
        Ok(Some(folder))
    }

    /// Public because library actions need the same standing admin identity music-folder
    /// detection does, and they need it from a background worker. Still best-effort: `None`
    /// when there are neither configured admin credentials nor a captured admin login.
    pub async fn ensure_admin_jwt(&self) -> Option<String> {
        self.ensure_jwt().await
    }

    /// Forget an admin JWT Navidrome refused, so the next `ensure_admin_jwt` logs in again.
    ///
    /// Nothing else ever cleared it apart from folder detection, which only runs at boot, on a
    /// dashboard load or on a captured login, while Navidrome's SessionTimeout defaults to 48
    /// hours. After two days every native call answered 401, read as "nothing there", and
    /// library actions stalled without a word. Clears only the token the caller used, so a
    /// concurrent caller that already logged in again keeps its fresh one.
    pub fn invalidate_admin_jwt(&self, stale_token: &str) {
        let mut state = self.inner.state.lock();
        if state.jwt.as_deref() == Some(stale_token) {
            state.jwt = None;
        }
    }

    /// True when either credential route is currently usable. Feature gates read this at
    /// startup so a missing credential is one clear log line, not one silent failure per
    /// action.
    pub fn has_admin_identity(&self) -> bool {
        if self
            .inner
            .state
            .lock()
            .jwt
            .as_deref()
            .is_some_and(|j| !j.is_empty())
        {
            return true;
        }
        let settings = self.inner.settings.current();
        settings
            .subsonic
            .admin_username
            .as_deref()
            .is_some_and(|u| !u.is_empty())
            && settings
                .subsonic
                .admin_password
                .as_deref()
                .is_some_and(|p| !p.is_empty())
    }

    /// Ensure a usable admin JWT: a captured one, else a fresh login with configured admin
    /// creds. Returns the token or `None` when neither is available.
    async fn ensure_jwt(&self) -> Option<String> {
        if let Some(jwt) = self.inner.state.lock().jwt.clone().filter(|j| !j.is_empty()) {
            return Some(jwt);
        }
        let settings = self.inner.settings.current();
        let subsonic = &settings.subsonic;
        let (Some(user), Some(password), Some(url)) = (
            subsonic.admin_username.as_deref().filter(|v| !v.is_empty()),
            subsonic.admin_password.as_deref().filter(|v| !v.is_empty()),
            subsonic.url.as_deref().filter(|v| !v.is_empty()),
        ) else {
            return None;
        };

        let payload =
            octo_core::json::to_string(&serde_json::json!({ "username": user, "password": password }));
        let result = self
            .inner
            .http
            .post(format!("{}/auth/login", url.trim_end_matches('/')))
            .header("Content-Type", "application/json; charset=utf-8")
            .body(payload)
            .send()
            .await;
        let response = match result {
            Ok(response) => response,
            Err(e) => {
                warn!("Navidrome admin login error: {e}");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(
                "Navidrome admin login failed: HTTP {}",
                response.status().as_u16()
            );
            return None;
        }
        match response.bytes().await {
            Ok(body) => self.capture_login(&body),
            Err(e) => {
                warn!("Navidrome admin login error: {e}");
                return None;
            }
        }
        self.inner.state.lock().jwt.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octo_core::settings::{AppSettings, SubsonicSettings};
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn service(url: &str, admin: Option<(&str, &str)>, library_path: &str) -> NavidromeIdentityService {
        let settings = SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                url: Some(url.to_string()),
                admin_username: admin.map(|a| a.0.to_string()),
                admin_password: admin.map(|a| a.1.to_string()),
                library_path: library_path.to_string(),
                ..Default::default()
            },
            ..Default::default()
        });
        NavidromeIdentityService::new(
            Arc::new(settings),
            crate::services::http_client_factory::default_client(),
        )
    }

    #[tokio::test]
    async fn only_an_admin_login_becomes_the_standing_identity() {
        let ids = service("http://navidrome.invalid", None, "");
        ids.capture_login(br#"{"token":"user-jwt","isAdmin":false,"username":"bob","subsonicToken":"t","subsonicSalt":"s"}"#);
        assert!(!ids.has_admin_identity());
        assert_eq!(ids.get_scan_auth(), None);
        assert_eq!(
            ids.username_for_native_token(Some("user-jwt")).as_deref(),
            Some("bob")
        );

        ids.capture_login(br#"{"token":"admin-jwt","isAdmin":true,"username":"admin","subsonicToken":"t1","subsonicSalt":"s1"}"#);
        assert!(ids.has_admin_identity());
        assert_eq!(
            ids.get_scan_auth(),
            Some(("admin".into(), "t1".into(), "s1".into()))
        );
        ids.invalidate_admin_jwt("someone-else");
        assert!(ids.has_admin_identity());
        ids.invalidate_admin_jwt("admin-jwt");
        assert!(!ids.has_admin_identity());

        // Not a login answer: nothing changes.
        ids.capture_login(b"<html/>");
        ids.capture_login(br#"{"token":5,"isAdmin":true}"#);
        assert_eq!(ids.username_for_native_token(Some(" ")), None);
    }

    #[tokio::test]
    async fn native_tokens_are_remembered_a_hundred_at_a_time() {
        let ids = service("http://navidrome.invalid", None, "");
        for i in 0..101 {
            ids.capture_login(format!(r#"{{"token":"t{i}","username":"u{i}"}}"#).as_bytes());
        }
        assert_eq!(ids.username_for_native_token(Some("t0")), None);
        assert_eq!(
            ids.username_for_native_token(Some("t100")).as_deref(),
            Some("u100")
        );
    }

    #[tokio::test]
    async fn detection_logs_in_and_adopts_a_mounted_library() {
        let music = tempfile::tempdir().expect("temp dir");
        let mounted = music.path().to_string_lossy().into_owned();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .and(body_json(
                serde_json::json!({ "username": "admin", "password": "pw" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "jwt-1", "isAdmin": true, "username": "admin",
                "subsonicToken": "tok", "subsonicSalt": "salt",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/library"))
            .and(header("X-Nd-Authorization", "Bearer jwt-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": 1, "name": "Elsewhere", "path": "/not/mounted/here" },
                { "id": 2, "name": "Music", "path": "/music", "remotePath": mounted },
            ])))
            .mount(&server)
            .await;

        // Pinned to the second library, which is mounted here.
        let ids = service(&server.uri(), Some(("admin", "pw")), &mounted);
        assert_eq!(
            ids.detect_music_folder(true).await.as_deref(),
            Some(mounted.as_str())
        );
        assert_eq!(ids.effective_download_path("./downloads"), mounted);
        let libraries = ids.known_libraries();
        assert_eq!(libraries.len(), 2);
        assert_eq!(libraries[0].id.as_deref(), Some("1"));
        assert_eq!(libraries[1].folder, mounted);
        assert_eq!(
            ids.get_scan_auth(),
            Some(("admin".into(), "tok".into(), "salt".into()))
        );

        // Unpinned, the first library wins, and it is not mounted here: nothing is adopted.
        let ids = service(&server.uri(), Some(("admin", "pw")), "");
        assert_eq!(ids.detect_music_folder(true).await, None);
        assert_eq!(ids.effective_download_path("./downloads"), "./downloads");
    }

    #[tokio::test]
    async fn an_expired_token_is_forgotten() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/library"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let ids = service(&server.uri(), None, "");
        ids.capture_login(br#"{"token":"old","isAdmin":true,"username":"admin"}"#);
        // The capture starts a detection of its own; this one waits its turn.
        assert_eq!(ids.detect_music_folder(true).await, None);
        assert!(!ids.has_admin_identity());
    }
}
