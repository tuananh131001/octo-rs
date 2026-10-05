//! `AcquisitionEndpointTests.cs` and `LibraryActionEndpointTests.cs`: getAcquisitions,
//! getOpenSubsonicExtensions, getLibraryActions, getUpgrades and libraryAction as the Octo app
//! sees them through the Subsonic API, plus the stars that feed getAcquisitions.

use std::path::{Path, PathBuf};

use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, SoulseekSettings,
    SubsonicSettings,
};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::xml::XElement;
use regex::Regex;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::test_support_6a2::*;
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::library::LibraryActionState;

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

const SONG_ID: &str = "nd-teardrop";
const SONG_PATH: &str = "Massive Attack/Mezzanine/03 - Teardrop.flac";
const SONG_BYTES: &[u8] = b"fLaC not really, but the right size";

fn ok_json() -> ResponseTemplate {
    navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"#)
}

fn wrong_password() -> ResponseTemplate {
    navidrome_json(
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
    )
}

/// The extension list Navidrome answers, in the format asked for.
fn extensions_answer(request: &MockRequest, json_list: &str) -> ResponseTemplate {
    if query_of(request).get("f").map(String::as_str) == Some("json") {
        navidrome_json(&format!(
            r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.58.0","openSubsonic":true,"openSubsonicExtensions":{json_list}}}}}"#
        ))
    } else {
        ResponseTemplate::new(200).set_body_raw(
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome"><openSubsonicExtensions name="formPost"><versions>1</versions></openSubsonicExtensions></subsonic-response>"#
                .as_bytes()
                .to_vec(),
            "application/xml",
        )
    }
}

/// How many requests reached one of these paths.
async fn count(server: &MockServer, matches: impl Fn(&str) -> bool) -> usize {
    received_paths(server)
        .await
        .iter()
        .filter(|path| matches(path))
        .count()
}

async fn pings(server: &MockServer) -> usize {
    count(server, |p| p == "/rest/ping").await
}

/// `openSubsonicExtensions` of a JSON answer as name → versions.
fn extensions_of(envelope: &Value) -> Vec<(String, Vec<i64>)> {
    envelope["openSubsonicExtensions"]
        .as_array()
        .expect("an extension list")
        .iter()
        .map(|e| {
            (
                e["name"].as_str().expect("a name").to_string(),
                e["versions"]
                    .as_array()
                    .expect("versions")
                    .iter()
                    .map(|v| v.as_i64().expect("a version"))
                    .collect(),
            )
        })
        .collect()
}

fn extension<'a>(extensions: &'a [(String, Vec<i64>)], name: &str) -> Option<&'a Vec<i64>> {
    extensions.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// The `openSubsonicExtensions` children of an XML answer.
fn xml_extensions(text: &str) -> Vec<XElement> {
    let root = XElement::parse(text).expect("XML");
    root.elements()
        .filter(|e| e.name == "openSubsonicExtensions")
        .cloned()
        .collect()
}

fn first_version(element: &XElement) -> Option<String> {
    element
        .elements()
        .find(|e| e.name == "versions")
        .map(XElement::value)
}

fn sorted_keys(object: &Value) -> Vec<String> {
    let mut keys: Vec<String> = object.as_object().expect("an object").keys().cloned().collect();
    keys.sort();
    keys
}

// ---------------------------------------------------------------------------------------
// AcquisitionEndpointTests
// ---------------------------------------------------------------------------------------

/// Navidrome as far as these calls need it: a ping that accepts the token "good" for anyone,
/// a star that it records, and a short extension list in either format.
struct AcquisitionNavidrome;

impl Respond for AcquisitionNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        let query = query_of(request);
        if path.ends_with("/rest/star") || path.ends_with("/rest/star.view") {
            return ok_json();
        }
        if path.ends_with("/rest/ping") {
            return if query.get("t").map(String::as_str) == Some("good") {
                ok_json()
            } else {
                wrong_password()
            };
        }
        if path.ends_with("/rest/getOpenSubsonicExtensions") {
            return extensions_answer(
                request,
                r#"[{"name":"formPost","versions":[1]},{"name":"songLyrics","versions":[1]}]"#,
            );
        }
        ResponseTemplate::new(404)
    }
}

struct AcquisitionApp {
    state: AppState,
    app: App,
    navidrome: MockServer,
    _directory: tempfile::TempDir,
}

impl AcquisitionApp {
    async fn new(change: impl FnOnce(&mut AppSettings)) -> AcquisitionApp {
        let navidrome = MockServer::start().await;
        Mock::given(any())
            .respond_with(AcquisitionNavidrome)
            .mount(&navidrome)
            .await;
        let directory = tempfile::tempdir().expect("a temp dir");
        let mut settings = AppSettings {
            soulseek: SoulseekSettings {
                base_url: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            ..settings(&navidrome.uri())
        };
        change(&mut settings);
        let state = state_with(settings, |_| {});
        state
            .settings
            .set_raw("Library:DownloadPath", Some(&directory.path().to_string_lossy()));
        AcquisitionApp {
            app: app(&state),
            state,
            navidrome,
            _directory: directory,
        }
    }

    /// The stars Navidrome received, as `user:id`.
    async fn stars(&self) -> Vec<String> {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with("/rest/star") || r.url.path().ends_with("/rest/star.view"))
            .map(|r| {
                let q = query_of(r);
                format!(
                    "{}:{}",
                    q.get("u").cloned().unwrap_or_default(),
                    q.get("id").cloned().unwrap_or_default()
                )
            })
            .collect()
    }

    fn seed(&self) {
        let tracker = &self.state.acquisition_tracker;
        tracker.begin(
            "soulseek",
            "3kX9Qm",
            Some("3kX9Qm"),
            Some("alice"),
            Some("Daft Punk"),
            Some("Da Funk"),
            Some("Homework"),
        );
        tracker.transfer(
            "soulseek",
            "3kX9Qm",
            Some(12_180_000),
            Some(29_000_000),
            Some(42.0),
            Some("Soulseek"),
        );
        tracker.begin(
            "soulseek",
            "7bYz2p",
            Some("7bYz2p"),
            Some("bob"),
            Some("Air"),
            Some("Sexy Boy"),
            Some("Moon Safari"),
        );
    }

    fn register_teardrop(&self) -> String {
        self.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some("Teardrop".into()),
            album: Some("Mezzanine".into()),
            duration: Some(330),
            ..Default::default()
        })
    }
}

fn acq_auth(user: &str) -> String {
    auth(user, "good", "octo-android")
}

fn acquisitions_of(envelope: &Value) -> Vec<Value> {
    envelope["acquisitions"]["acquisition"]
        .as_array()
        .expect("a list of acquisitions")
        .clone()
}

#[tokio::test]
async fn get_acquisitions_returns_only_the_callers_rows_in_the_apps_shape() {
    let t = AcquisitionApp::new(|_| {}).await;
    t.seed();

    // No f=json: JSON regardless.
    let reply = get(
        &t.app,
        &format!("/rest/getAcquisitions.view?{}", acq_auth("alice")),
    )
    .await;
    assert_eq!(reply.media_type().as_deref(), Some("application/json"));
    let envelope = reply.envelope();

    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["version"], "1.16.1");
    assert_eq!(envelope["type"], "octo");

    let rows = acquisitions_of(&envelope);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];

    assert_eq!(
        sorted_keys(row),
        [
            "ahead",
            "album",
            "artist",
            "bytesDone",
            "bytesTotal",
            "error",
            "id",
            "libraryId",
            "note",
            "progress",
            "source",
            "startedAt",
            "state",
            "title",
            "updatedAt"
        ]
    );
    assert_eq!(row["id"], "3kX9Qm");
    assert_eq!(row["artist"], "Daft Punk");
    assert_eq!(row["title"], "Da Funk");
    assert_eq!(row["album"], "Homework");
    assert_eq!(row["state"], "downloading");
    assert_eq!(row["progress"].as_f64(), Some(0.42));
    assert_eq!(row["bytesDone"].as_i64(), Some(12_180_000));
    assert_eq!(row["bytesTotal"].as_i64(), Some(29_000_000));
    assert_eq!(row["source"], "Soulseek");
    let stamp = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").expect("regex");
    assert!(
        stamp.is_match(row["startedAt"].as_str().expect("startedAt")),
        "{row}"
    );
    assert!(
        stamp.is_match(row["updatedAt"].as_str().expect("updatedAt")),
        "{row}"
    );
    assert!(row["error"].is_null());
    assert!(row["libraryId"].is_null());
}

#[tokio::test]
async fn get_acquisitions_another_user_sees_only_their_own() {
    let t = AcquisitionApp::new(|_| {}).await;
    t.seed();

    let bob = get(
        &t.app,
        &format!("/rest/getAcquisitions?{}&f=json", acq_auth("bob")),
    )
    .await;
    let rows = acquisitions_of(&bob.envelope());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], "7bYz2p");
    assert_eq!(rows[0]["state"], "queued");
    assert!(rows[0]["progress"].is_null());

    let carol = get(
        &t.app,
        &format!("/rest/getAcquisitions?{}&f=json", acq_auth("carol")),
    )
    .await;
    assert!(acquisitions_of(&carol.envelope()).is_empty());
}

#[tokio::test]
async fn starring_a_found_song_lists_it_for_the_starrer_under_the_starred_id() {
    let t = AcquisitionApp::new(|_| {}).await;
    let id = t.register_teardrop();

    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={id}", acq_auth("alice")),
    )
    .await;
    assert!(star.status.is_success());

    // No worker runs in this app, so the heart waits in the queue, which is the point.
    let reply = get(
        &t.app,
        &format!("/rest/getAcquisitions.view?{}", acq_auth("alice")),
    )
    .await;
    let rows = acquisitions_of(&reply.envelope());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], id.as_str());
    assert_eq!(rows[0]["state"], "queued");
    assert_eq!(rows[0]["artist"], "Massive Attack");
    assert_eq!(rows[0]["title"], "Teardrop");
    assert!(t.state.acquisition_tracker.for_user("bob").is_empty());
}

/// Stars Teardrop from `client` and answers how many sign-ins StarOnArrival then holds.
async fn held_after_star(t: &AcquisitionApp, client: &str) -> usize {
    let id = t.register_teardrop();

    let reply = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={id}", auth("alice", "good", client)),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    // The download is asked for either way; only the favorite depends on who starred it.
    assert_eq!(t.state.acquisition_tracker.for_user("alice").len(), 1);
    t.state.star_on_arrival.held()
}

/// A song already in Navidrome carries Navidrome's own id, so its heart is Navidrome's
/// favorite, sent on as the person who hearted it, and nothing downloads.
#[tokio::test]
async fn starring_a_library_song_is_a_navidrome_favorite_and_downloads_nothing() {
    let t = AcquisitionApp::new(|_| {}).await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/star.view?{}&f=json&id=nd-42",
            auth("alice", "good", "Symfonium")
        ),
    )
    .await;

    assert!(reply.status.is_success());
    assert_eq!(t.stars().await, ["alice:nd-42"]);
    assert!(t.state.track_acquisition_queue.is_idle());
    assert!(t.state.acquisition_tracker.all().is_empty());
    assert_eq!(t.state.star_on_arrival.held(), 0);
}

/// Octo's own apps sync their favorites as stars on library songs. Those reach Navidrome
/// exactly as before: the heart rules for outside songs never touch them.
#[tokio::test]
async fn starring_a_library_song_from_the_octo_app_is_relayed_as_before() {
    let t = AcquisitionApp::new(|_| {}).await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/star.view?{}&f=json&id=nd-42",
            auth("alice", "good", "Octo")
        ),
    )
    .await;

    assert!(reply.status.is_success());
    assert_eq!(t.stars().await, ["alice:nd-42"]);
    assert!(t.state.track_acquisition_queue.is_idle());
    assert_eq!(t.state.star_on_arrival.held(), 0);
}

#[tokio::test]
async fn star_from_the_octo_app_holds_no_sign_in() {
    let t = AcquisitionApp::new(|_| {}).await;
    assert_eq!(held_after_star(&t, "Octo").await, 0);
}

#[tokio::test]
async fn star_from_another_client_holds_the_sign_in() {
    let t = AcquisitionApp::new(|_| {}).await;
    assert_eq!(held_after_star(&t, "Symfonium").await, 1);
}

#[tokio::test]
async fn star_with_download_favorites_off_still_holds_the_sign_in() {
    // The song may turn out to be in the library already, and that heart is always a favorite.
    let t = AcquisitionApp::new(|s| s.subsonic.star_downloads_for_requester = false).await;
    assert_eq!(held_after_star(&t, "Symfonium").await, 1);
}

#[tokio::test]
async fn get_acquisitions_wrong_credentials_reveal_nothing() {
    let t = AcquisitionApp::new(|_| {}).await;
    t.seed();

    let reply = get(
        &t.app,
        &format!(
            "/rest/getAcquisitions.view?{}",
            auth("alice", "bad", "octo-android")
        ),
    )
    .await;

    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "failed");
    assert_eq!(envelope["error"]["code"], 40);
    assert!(envelope.get("acquisitions").is_none());
    assert_eq!(pings(&t.navidrome).await, 1);
}

#[tokio::test]
async fn get_open_subsonic_extensions_adds_octo_acquisitions_to_navidromes_list() {
    let t = AcquisitionApp::new(|_| {}).await;

    let reply = get(
        &t.app,
        "/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=octo-android",
    )
    .await;

    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["openSubsonic"], true);
    let extensions = extensions_of(&envelope);
    assert_eq!(extension(&extensions, "octoAcquisitions"), Some(&vec![1]));
    assert!(extension(&extensions, "formPost").is_some());
    assert!(extension(&extensions, "songLyrics").is_some());
}

#[tokio::test]
async fn get_open_subsonic_extensions_adds_it_in_xml_too() {
    let t = AcquisitionApp::new(|_| {}).await;

    let reply = get(&t.app, "/rest/getOpenSubsonicExtensions?v=1.16.1&c=test").await;

    let extensions = xml_extensions(&reply.text());
    assert!(extensions.iter().any(|e| e.attribute("name") == Some("formPost")));
    let ours: Vec<_> = extensions
        .iter()
        .filter(|e| e.attribute("name") == Some("octoAcquisitions"))
        .collect();
    assert_eq!(ours.len(), 1);
    assert_eq!(first_version(ours[0]).as_deref(), Some("1"));
}

// `AdminAcquisitions_ListsEveryonesRowsWithWhoAsked` drives `/api/admin/acquisitions`
// (AdminController): it belongs to 6-B.

// ---------------------------------------------------------------------------------------
// LibraryActionEndpointTests
// ---------------------------------------------------------------------------------------

/// Navidrome as far as these calls need it: a ping that accepts the token "good" or the API
/// key "goodkey", an admin login for Octo's own identity, one song the native API knows, and
/// a short extension list in either format.
struct LibraryNavidrome {
    library_path: String,
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        let query = query_of(request);
        if path.ends_with("/rest/ping") {
            let ok = query.get("t").map(String::as_str) == Some("good")
                || query.get("apiKey").map(String::as_str) == Some("goodkey");
            return if ok { ok_json() } else { wrong_password() };
        }
        if path == "/auth/login" {
            return navidrome_json(
                r#"{"token":"admin-jwt","isAdmin":true,"username":"admin","subsonicToken":"st","subsonicSalt":"ss"}"#,
            );
        }
        if path.starts_with("/api/song/") || path.ends_with("/rest/getSong") {
            if path == format!("/api/song/{SONG_ID}") {
                return navidrome_json(
                    &json!({
                        "id": SONG_ID, "path": SONG_PATH, "libraryPath": self.library_path,
                        "size": SONG_BYTES.len(), "suffix": "flac", "title": "Teardrop",
                        "artist": "Massive Attack", "album": "Mezzanine", "duration": 330,
                    })
                    .to_string(),
                );
            }
            return ResponseTemplate::new(404);
        }
        if path.ends_with("/rest/getOpenSubsonicExtensions") {
            return extensions_answer(request, r#"[{"name":"formPost","versions":[1]}]"#);
        }
        ResponseTemplate::new(404)
    }
}

/// A real executor, resolver and quarantine over a temporary music folder. Library actions
/// are on, real (no dry run), Delete is on and alice is allowed, unless a test says otherwise.
struct LibraryApp {
    state: AppState,
    app: App,
    navidrome: MockServer,
    directory: tempfile::TempDir,
}

impl LibraryApp {
    async fn new(change: impl FnOnce(&mut AppSettings)) -> LibraryApp {
        let directory = tempfile::tempdir().expect("a temp dir");
        let song = directory.path().join(SONG_PATH);
        std::fs::create_dir_all(song.parent().expect("a folder")).expect("folders");
        std::fs::write(&song, SONG_BYTES).expect("written");

        let navidrome = MockServer::start().await;
        Mock::given(any())
            .respond_with(LibraryNavidrome {
                library_path: directory.path().to_string_lossy().into_owned(),
            })
            .mount(&navidrome)
            .await;

        let mut settings = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                auto_detect_download_path: false,
                admin_username: Some("admin".into()),
                admin_password: Some("admin-password".into()),
                ..Default::default()
            },
            soulseek: SoulseekSettings {
                base_url: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run: false,
                allowed_users: vec!["alice".into()],
                actions: vec![LibraryActionDefinition {
                    action: LibraryAction::Delete,
                    enabled: true,
                    ..Default::default()
                }],
                quarantine_retention_days: 14,
                ..Default::default()
            },
            ..Default::default()
        };
        change(&mut settings);
        let state = state_with(settings, |_| {});
        state
            .settings
            .set_raw("Library:DownloadPath", Some(&directory.path().to_string_lossy()));
        LibraryApp {
            app: app(&state),
            state,
            navidrome,
            directory,
        }
    }

    fn song_file(&self) -> PathBuf {
        self.directory.path().join(SONG_PATH)
    }

    fn quarantine(&self) -> PathBuf {
        self.directory.path().join(".octo-trash")
    }

    async fn song_lookups(&self) -> usize {
        count(&self.navidrome, |p| {
            p.starts_with("/api/song/") || p.ends_with("/rest/getSong")
        })
        .await
    }

    /// Nothing moved, nothing looked up, nothing written to the journal.
    async fn assert_untouched(&self) {
        assert!(self.song_file().exists());
        assert!(!self.quarantine().exists());
        assert_eq!(self.song_lookups().await, 0);
        assert!(self.state.library_action_journal.recent(200).is_empty());
    }

    /// GETs `url`, which must answer JSON, and reads it.
    async fn get_json(&self, url: &str) -> Value {
        let reply = get(&self.app, url).await;
        assert_eq!(reply.media_type().as_deref(), Some("application/json"), "{url}");
        reply.json()
    }
}

fn lib_auth(user: &str) -> String {
    auth(user, "good", "octo-android")
}

fn envelope(doc: &Value) -> &Value {
    &doc["subsonic-response"]
}

/// The `libraryAction` of an ok answer, checking the envelope and its exact keys.
fn action(doc: &Value) -> Value {
    let envelope = envelope(doc);
    assert_eq!(envelope["status"], "ok", "{doc}");
    assert_eq!(envelope["type"], "octo");
    assert_eq!(envelope["openSubsonic"], true);
    let action = envelope["libraryAction"].clone();
    assert_eq!(sorted_keys(&action), ["action", "detail", "id", "state"]);
    action
}

fn assert_failed(doc: &Value, code: i64) {
    let envelope = envelope(doc);
    assert_eq!(envelope["status"], "failed", "{doc}");
    assert_eq!(envelope["error"]["code"].as_i64(), Some(code), "{doc}");
    assert!(envelope.get("libraryAction").is_none());
    assert!(envelope.get("libraryActions").is_none());
}

fn files_under(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path, extension));
        } else if path.extension().is_some_and(|e| e == extension) {
            found.push(path);
        }
    }
    found
}

// getOpenSubsonicExtensions

#[tokio::test]
async fn extensions_list_octo_library_actions_only_while_they_are_on() {
    for (enabled, listed) in [(true, true), (false, false)] {
        let t = LibraryApp::new(|s| s.library_actions.enabled = enabled).await;

        let reply = get(
            &t.app,
            "/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=octo-android",
        )
        .await;

        let extensions = extensions_of(&reply.envelope());
        assert!(extension(&extensions, "formPost").is_some(), "enabled={enabled}");
        assert!(
            extension(&extensions, "octoAcquisitions").is_some(),
            "enabled={enabled}"
        );
        if listed {
            assert_eq!(
                extension(&extensions, "octoLibraryActions"),
                Some(&vec![1, 2]),
                "enabled={enabled}"
            );
        } else {
            assert!(
                extension(&extensions, "octoLibraryActions").is_none(),
                "enabled={enabled}"
            );
        }
    }
}

#[tokio::test]
async fn extensions_list_octo_library_actions_only_while_they_are_on_in_xml_too() {
    for (enabled, listed) in [(true, true), (false, false)] {
        let t = LibraryApp::new(|s| s.library_actions.enabled = enabled).await;

        let reply = get(&t.app, "/rest/getOpenSubsonicExtensions?v=1.16.1&c=test").await;

        let extensions = xml_extensions(&reply.text());
        let ours: Vec<_> = extensions
            .iter()
            .filter(|e| e.attribute("name") == Some("octoLibraryActions"))
            .collect();
        assert!(
            extensions.iter().any(|e| e.attribute("name") == Some("formPost")),
            "enabled={enabled}"
        );
        if listed {
            assert_eq!(ours.len(), 1, "enabled={enabled}");
            assert_eq!(first_version(ours[0]).as_deref(), Some("1"), "enabled={enabled}");
        } else {
            assert!(ours.is_empty(), "enabled={enabled}");
        }
    }
}

// getLibraryActions

#[tokio::test]
async fn get_library_actions_describes_what_the_caller_may_do_in_the_apps_shape() {
    let t = LibraryApp::new(|_| {}).await;

    // No f=json: JSON regardless.
    let doc = t
        .get_json(&format!("/rest/getLibraryActions.view?{}", lib_auth("alice")))
        .await;

    let envelope = envelope(&doc);
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["version"], "1.16.1");
    assert_eq!(envelope["type"], "octo");
    assert_eq!(envelope["openSubsonic"], true);
    let actions = &envelope["libraryActions"];
    assert_eq!(
        sorted_keys(actions),
        [
            "actions",
            "allowed",
            "dryRun",
            "enabled",
            "keepDays",
            "parallel",
            "upgradeSource"
        ]
    );
    assert_eq!(actions["enabled"], true);
    assert_eq!(actions["allowed"], true);
    assert_eq!(actions["dryRun"], false);
    assert_eq!(actions["actions"], json!(["remove"]));
    assert_eq!(actions["keepDays"], 14);
}

#[tokio::test]
async fn get_library_actions_someone_not_on_the_allowlist_is_not_allowed() {
    let t = LibraryApp::new(|_| {}).await;

    let doc = t
        .get_json(&format!("/rest/getLibraryActions?{}&f=json", lib_auth("bob")))
        .await;

    let actions = &envelope(&doc)["libraryActions"];
    assert_eq!(actions["enabled"], true);
    assert_eq!(actions["allowed"], false);
}

#[tokio::test]
async fn get_library_actions_dry_run_on_delete_off_kept_forever() {
    let t = LibraryApp::new(|s| {
        s.library_actions.dry_run = true;
        s.library_actions.actions[0].enabled = false;
        s.library_actions.quarantine_retention_days = 0;
    })
    .await;

    let doc = t
        .get_json(&format!("/rest/getLibraryActions?{}", lib_auth("alice")))
        .await;

    let actions = &envelope(&doc)["libraryActions"];
    assert_eq!(actions["dryRun"], true);
    assert_eq!(actions["actions"], json!([]));
    assert_eq!(actions["keepDays"], 0);
}

#[tokio::test]
async fn get_library_actions_off_says_so() {
    let t = LibraryApp::new(|s| s.library_actions.enabled = false).await;

    let doc = t
        .get_json(&format!("/rest/getLibraryActions?{}", lib_auth("alice")))
        .await;

    assert_eq!(envelope(&doc)["libraryActions"]["enabled"], false);
}

#[tokio::test]
async fn get_library_actions_wrong_password_is_error40() {
    let t = LibraryApp::new(|_| {}).await;

    let doc = t
        .get_json(&format!(
            "/rest/getLibraryActions.view?{}",
            auth("alice", "bad", "octo-android")
        ))
        .await;

    assert_failed(&doc, 40);
    assert_eq!(pings(&t.navidrome).await, 1);
}

// libraryAction

#[tokio::test]
async fn remove_moves_the_file_to_quarantine_as_delete_for_the_caller() {
    let t = LibraryApp::new(|_| {}).await;

    // A form post, as formPost clients send it.
    let form = format!("u=alice&t=good&s=salt&v=1.16.1&c=octo-android&id={SONG_ID}&action=remove");
    let reply = send(
        &t.app,
        axum::http::Method::POST,
        "/rest/libraryAction.view",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        form,
    )
    .await;
    assert_eq!(reply.media_type().as_deref(), Some("application/json"));
    let doc = reply.json();

    let action = action(&doc);
    assert_eq!(action["id"], SONG_ID);
    assert_eq!(action["action"], "remove");
    assert_eq!(action["state"], "applied", "{doc}");
    assert_eq!(action["detail"], "Removed. It will not be downloaded again.");

    assert!(!t.song_file().exists());
    let moved = files_under(&t.quarantine(), "flac");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(std::fs::read(&moved[0]).expect("readable"), SONG_BYTES);

    let journal = t.state.library_action_journal.recent(200);
    assert_eq!(journal.len(), 1, "{journal:?}");
    let entry = &journal[0];
    assert_eq!(entry.action, LibraryAction::Delete);
    assert_eq!(entry.navidrome_id, SONG_ID);
    assert_eq!(entry.username, "alice");
    assert_eq!(entry.state, LibraryActionState::Applied);
    assert!(!entry.dry_run);
}

#[tokio::test]
async fn remove_in_a_dry_run_is_rehearsed_and_moves_nothing() {
    let t = LibraryApp::new(|s| s.library_actions.dry_run = true).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?{}&id={SONG_ID}&action=remove",
            lib_auth("alice")
        ))
        .await;

    let action = action(&doc);
    assert_eq!(action["state"], "rehearsed", "{doc}");
    assert!(
        action["detail"]
            .as_str()
            .expect("a detail")
            .starts_with("Dry run: would remove "),
        "{doc}"
    );
    assert!(t.song_file().exists());
    assert!(!t.quarantine().exists());
    let journal = t.state.library_action_journal.recent(200);
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].state, LibraryActionState::Rehearsed);
}

#[tokio::test]
async fn remove_by_someone_not_on_the_allowlist_is_skipped_and_moves_nothing() {
    let t = LibraryApp::new(|_| {}).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?{}&id={SONG_ID}&action=remove",
            lib_auth("bob")
        ))
        .await;

    let action = action(&doc);
    assert_eq!(action["state"], "skipped");
    assert_eq!(action["detail"], "bob is not on the allowlist.");
    t.assert_untouched().await;
}

#[tokio::test]
async fn remove_with_library_actions_off_is_skipped_and_moves_nothing() {
    let t = LibraryApp::new(|s| s.library_actions.enabled = false).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?{}&id={SONG_ID}&action=remove",
            lib_auth("alice")
        ))
        .await;

    assert_eq!(action(&doc)["state"], "skipped");
    t.assert_untouched().await;
}

#[tokio::test]
async fn remove_with_delete_off_is_skipped_and_moves_nothing() {
    let t = LibraryApp::new(|s| s.library_actions.actions[0].enabled = false).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?{}&id={SONG_ID}&action=remove",
            lib_auth("alice")
        ))
        .await;

    assert_eq!(action(&doc)["state"], "skipped");
    t.assert_untouched().await;
}

#[tokio::test]
async fn remove_of_a_song_with_no_file_it_can_prove_is_unresolved() {
    let t = LibraryApp::new(|_| {}).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?{}&id=nd-unknown&action=remove",
            lib_auth("alice")
        ))
        .await;

    let action = action(&doc);
    assert_eq!(action["id"], "nd-unknown");
    assert_eq!(action["state"], "unresolved", "{doc}");
    assert!(t.song_file().exists());
    assert!(!t.quarantine().exists());
}

#[tokio::test]
async fn remove_without_id_or_action_is_error10() {
    for query in [
        "action=remove".to_string(),
        format!("id={SONG_ID}"),
        "id=&action=remove".to_string(),
    ] {
        let t = LibraryApp::new(|_| {}).await;

        let doc = t
            .get_json(&format!("/rest/libraryAction.view?{}&{query}", lib_auth("alice")))
            .await;

        assert_failed(&doc, 10);
        t.assert_untouched().await;
    }
}

#[tokio::test]
async fn remove_any_other_action_is_an_error_and_moves_nothing() {
    for verb in ["delete", "rate", "wrongSong"] {
        let t = LibraryApp::new(|_| {}).await;

        let doc = t
            .get_json(&format!(
                "/rest/libraryAction?{}&id={SONG_ID}&action={verb}",
                lib_auth("alice")
            ))
            .await;

        assert_failed(&doc, 0);
        assert!(
            envelope(&doc)["error"]["message"]
                .as_str()
                .expect("a message")
                .contains(verb),
            "{verb}: {doc}"
        );
        t.assert_untouched().await;
    }
}

#[tokio::test]
async fn remove_with_a_wrong_password_is_error40_and_never_reaches_the_executor() {
    let t = LibraryApp::new(|_| {}).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?{}&id={SONG_ID}&action=remove",
            auth("alice", "bad", "octo-android")
        ))
        .await;

    assert_failed(&doc, 40);
    assert_eq!(pings(&t.navidrome).await, 1);
    t.assert_untouched().await;
}

#[tokio::test]
async fn remove_with_only_an_api_key_is_skipped_because_it_names_nobody() {
    let t = LibraryApp::new(|_| {}).await;

    let doc = t
        .get_json(&format!(
            "/rest/libraryAction?apiKey=goodkey&v=1.16.1&c=octo-android&id={SONG_ID}&action=remove"
        ))
        .await;

    let action = action(&doc);
    assert_eq!(action["state"], "skipped");
    assert!(action["detail"].as_str().expect("a detail").contains("API key"));
    assert_eq!(pings(&t.navidrome).await, 1);
    t.assert_untouched().await;
}

// octoLibraryActions v2: upgrade and getUpgrades

/// Better quality searches Soulseek, so it is offered only where slskd is set up.
fn better_quality_on(settings: &mut AppSettings, dry_run: bool, slskd: bool) {
    let login = |value: &str| Some(if slskd { value.to_string() } else { String::new() });
    settings.soulseek.username = login("slskd-user");
    settings.soulseek.password = login("slskd-pass");
    settings.library_actions.actions.push(LibraryActionDefinition {
        action: LibraryAction::BetterQuality,
        enabled: true,
        ..Default::default()
    });
    settings.library_actions.dry_run = dry_run;
}

fn upgrade_url(user: &str) -> String {
    format!(
        "/rest/libraryAction.view?id=nd-1&action=upgrade&{}",
        lib_auth(user)
    )
}

#[tokio::test]
async fn get_library_actions_lists_upgrade_only_while_better_quality_is_on() {
    {
        let off = LibraryApp::new(|_| {}).await;
        let doc = off
            .get_json(&format!("/rest/getLibraryActions.view?{}", lib_auth("alice")))
            .await;
        let actions = &envelope(&doc)["libraryActions"];
        assert_eq!(actions["actions"], json!(["remove"]));
        assert_eq!(actions["parallel"], 1);
    }
    {
        let on = LibraryApp::new(|s| better_quality_on(s, false, true)).await;
        let doc = on
            .get_json(&format!("/rest/getLibraryActions.view?{}", lib_auth("alice")))
            .await;
        assert_eq!(
            envelope(&doc)["libraryActions"]["actions"],
            json!(["remove", "upgrade"])
        );
    }
}

#[tokio::test]
async fn upgrade_is_queued_at_once_and_listed_for_the_caller_first() {
    let t = LibraryApp::new(|s| better_quality_on(s, false, true)).await;

    let started = std::time::Instant::now();
    let doc = t.get_json(&upgrade_url("alice")).await;
    let elapsed = started.elapsed();

    let action = action(&doc);
    assert_eq!(action["id"], "nd-1");
    assert_eq!(action["action"], "upgrade");
    assert_eq!(action["state"], "queued", "{doc}");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "answered after {elapsed:?}"
    );
    let jobs = t.state.upgrade_queue.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].requested_by, "alice");
    assert_eq!(jobs[0].origin, "app");
    t.assert_untouched().await;

    let listed = t
        .get_json(&format!("/rest/getUpgrades.view?{}", lib_auth("alice")))
        .await;
    let rows = envelope(&listed)["upgrades"]
        .as_array()
        .expect("upgrades")
        .clone();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["id"], "nd-1");
    assert_eq!(rows[0]["state"], "queued");
    assert_eq!(
        sorted_keys(&rows[0]),
        [
            "album",
            "artist",
            "detail",
            "id",
            "progress",
            "state",
            "title",
            "updatedAt"
        ]
    );

    let someone_else = t
        .get_json(&format!("/rest/getUpgrades.view?{}", lib_auth("bob")))
        .await;
    assert_eq!(envelope(&someone_else)["upgrades"], json!([]));
}

#[tokio::test]
async fn upgrade_that_could_not_change_anything_is_skipped_with_the_reason() {
    for (user, dry_run, reason) in [("bob", false, "allowed list"), ("alice", true, "dry run")] {
        let t = LibraryApp::new(|s| better_quality_on(s, dry_run, true)).await;
        let doc = t.get_json(&upgrade_url(user)).await;
        let action = action(&doc);
        assert_eq!(action["state"], "skipped", "{user}");
        assert!(
            action["detail"].as_str().expect("a detail").contains(reason),
            "{user}: {doc}"
        );
        assert!(t.state.upgrade_queue.snapshot().is_empty(), "{user}");
    }
}

#[tokio::test]
async fn upgrade_with_better_quality_off_is_skipped() {
    let t = LibraryApp::new(|_| {}).await;
    let doc = t.get_json(&upgrade_url("alice")).await;
    assert!(
        action(&doc)["detail"]
            .as_str()
            .expect("a detail")
            .contains("Better quality")
    );
    assert!(t.state.upgrade_queue.snapshot().is_empty());
}

#[tokio::test]
async fn without_slskd_set_up_upgrade_is_not_offered_and_asking_is_skipped() {
    let t = LibraryApp::new(|s| better_quality_on(s, false, false)).await;
    let actions = t
        .get_json(&format!("/rest/getLibraryActions.view?{}", lib_auth("alice")))
        .await;
    let described = &envelope(&actions)["libraryActions"];
    assert!(
        !described["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .any(|a| a == "upgrade")
    );
    assert!(described["upgradeSource"].is_null());

    let doc = t.get_json(&upgrade_url("alice")).await;
    let action = action(&doc);
    assert_eq!(action["state"], "skipped");
    assert!(
        action["detail"]
            .as_str()
            .expect("a detail")
            .contains("not set up")
    );
    assert!(t.state.upgrade_queue.snapshot().is_empty());
}

#[tokio::test]
async fn get_library_actions_names_where_an_upgrade_looks() {
    let t = LibraryApp::new(|s| better_quality_on(s, false, true)).await;
    let doc = t
        .get_json(&format!("/rest/getLibraryActions.view?{}", lib_auth("alice")))
        .await;
    assert_eq!(envelope(&doc)["libraryActions"]["upgradeSource"], "Soulseek");
}

#[tokio::test]
async fn get_upgrades_wrong_password_is_error40() {
    let t = LibraryApp::new(|s| better_quality_on(s, false, true)).await;
    let doc = t
        .get_json(&format!(
            "/rest/getUpgrades.view?{}",
            auth("alice", "wrong", "octo-android")
        ))
        .await;
    let envelope = envelope(&doc);
    assert_eq!(envelope["status"], "failed");
    assert_eq!(envelope["error"]["code"], 40);
}
