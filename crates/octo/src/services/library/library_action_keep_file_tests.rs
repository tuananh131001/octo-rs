//! The rest of `LibraryActionKeepTests.cs` whose subjects are already ported: the
//! `MusicBrainzRecordingPickTests` (2-D's `music_brainz_client::pick`), `AcoustIdSubmissionTests`'
//! `BuildSubmitForm_NumbersEachItemAndLeavesOutAnUnknownFormat` (2-D's `build_submit_form`) and
//! the `NavidromePlaylistApiTests` (3-E's `NavidromePlaylistApi`). The file's other classes test
//! `NoticePlaylistWorker` (5-B) and `SubSonicController` (6-A).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::fingerprint::acoust_id_client::{AcoustIdSubmission, build_submit_form};
use octo_core::fingerprint::music_brainz_client::pick;
use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::NavidromePlaylistApi;
use crate::services::subsonic::NavidromeIdentityService;

// ---- MusicBrainzRecordingPickTests ---------------------------------------------------------
//
// The MusicBrainz recording a kept fingerprint is submitted with. MusicBrainz holds
// near-duplicates, so anything but exactly one fit is no answer: a guess is never sent with
// someone's name on it.

fn recording(id: &str, title: &str, length_ms: i64) -> Value {
    recording_with(id, title, length_ms, "Massive Attack", 100, "", false)
}

fn recording_with(
    id: &str,
    title: &str,
    length_ms: i64,
    artist: &str,
    score: i32,
    disambiguation: &str,
    video: bool,
) -> Value {
    json!({
        "id": id, "score": score, "title": title, "length": length_ms, "video": video,
        "disambiguation": disambiguation, "artist-credit": [{"name": artist}],
    })
}

fn picked(recordings: Vec<Value>) -> Option<String> {
    pick(
        &json!({ "recordings": recordings }),
        "Massive Attack",
        "Teardrop",
        315,
    )
    .expect("readable")
}

/// The real pair: two "Teardrop" recordings by Massive Attack, 27 ms apart.
#[test]
fn pick_two_recordings_that_both_fit_is_no_answer() {
    assert_eq!(
        picked(vec![
            recording("f200a9a9", "Teardrop", 314813),
            recording("b39f9fe4", "Teardrop", 314786),
        ]),
        None
    );
}

#[test]
fn pick_other_versions_do_not_count_so_one_fit_is_the_answer() {
    assert_eq!(
        picked(vec![
            recording("f200a9a9", "Teardrop", 314813),
            recording_with(
                "live",
                "Teardrop",
                316000,
                "Massive Attack",
                100,
                "live, 1998-12-05: Brixton Academy",
                false
            ),
            recording("remix", "Teardrop (Mad Professor mix)", 314000),
        ])
        .as_deref(),
        Some("f200a9a9")
    );
}

#[test]
fn pick_ignores_weak_scores_videos_other_artists_and_other_lengths() {
    assert_eq!(
        picked(vec![
            recording("keep", "Teardrop", 314813),
            recording_with("weak", "Teardrop", 314813, "Massive Attack", 80, "", false),
            recording_with("video", "Teardrop", 314813, "Massive Attack", 100, "", true),
            recording_with("cover", "Teardrop", 314813, "Elbow", 100, "", false),
            recording("long", "Teardrop", 330000),
        ])
        .as_deref(),
        Some("keep")
    );
}

#[test]
fn pick_no_recordings_is_no_answer() {
    assert_eq!(
        pick(&json!({}), "Massive Attack", "Teardrop", 315).expect("readable"),
        None
    );
}

// ---- AcoustIdSubmissionTests: the form -----------------------------------------------------

#[test]
fn build_submit_form_numbers_each_item_and_leaves_out_an_unknown_format() {
    let form = build_submit_form(
        "app",
        "user",
        &[
            AcoustIdSubmission {
                fingerprint: "AQAB1".into(),
                duration_seconds: 330,
                recording_id: "f200a9a9-6f0a-4a8b-9f5e-000000000001".into(),
                file_format: Some("flac".into()),
            },
            AcoustIdSubmission {
                fingerprint: "AQAB2".into(),
                duration_seconds: 200,
                recording_id: "b39f9fe4-6f0a-4a8b-9f5e-000000000002".into(),
                file_format: None,
            },
        ],
    );
    let field = |name: &str| form.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());

    assert_eq!(field("client"), Some("app"));
    assert_eq!(field("user"), Some("user"));
    assert_eq!(field("format"), Some("json"));
    assert!(field("clientversion").is_some_and(|v| v.starts_with("octo-")));
    assert_eq!(field("duration.0"), Some("330"));
    assert_eq!(field("fingerprint.0"), Some("AQAB1"));
    assert_eq!(field("mbid.0"), Some("f200a9a9-6f0a-4a8b-9f5e-000000000001"));
    assert_eq!(field("fileformat.0"), Some("flac"));
    assert_eq!(field("duration.1"), Some("200"));
    assert_eq!(field("fileformat.1"), None);
}

// ---- NavidromePlaylistApiTests -------------------------------------------------------------
//
// Navidrome's admin token lapses after SessionTimeout (48 hours by default). Before this, every
// native call answered 401 from then on, read as "nothing there", and library actions stalled
// without a word.

/// Logs in with a new token each time; the playlist calls answer what `playlists` says for the
/// bearer they carried; a call with a body records it; songs answer after `song_delay`.
#[derive(Clone)]
struct TokenNavidrome {
    logins: Arc<AtomicUsize>,
    bearers: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
    playlists: Arc<dyn Fn(&str) -> u16 + Send + Sync>,
    song_delay: Duration,
}

impl TokenNavidrome {
    fn new(playlists: impl Fn(&str) -> u16 + Send + Sync + 'static) -> Self {
        TokenNavidrome {
            logins: Arc::default(),
            bearers: Arc::default(),
            bodies: Arc::default(),
            playlists: Arc::new(playlists),
            song_delay: Duration::ZERO,
        }
    }
}

impl Respond for TokenNavidrome {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        if path == "/auth/login" {
            let login = self.logins.fetch_add(1, Ordering::SeqCst) + 1;
            return ResponseTemplate::new(200).set_body_json(json!({
                "token": format!("jwt-{login}"), "isAdmin": true, "username": "admin"
            }));
        }
        if path.starts_with("/api/song") {
            return ResponseTemplate::new(200)
                .set_body_json(json!([]))
                .set_delay(self.song_delay);
        }
        // A login also sets off a music-folder lookup; only the playlist calls are under test.
        if !path.starts_with("/api/playlist") {
            return ResponseTemplate::new(200).set_body_json(json!([]));
        }
        if !request.body.is_empty() {
            self.bodies
                .lock()
                .push(String::from_utf8_lossy(&request.body).into_owned());
            return ResponseTemplate::new(200).set_body_json(json!({"added": 2}));
        }
        let bearer = request
            .headers
            .get("X-Nd-Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        self.bearers.lock().push(bearer.clone());
        match (self.playlists)(&bearer) {
            200 => ResponseTemplate::new(200)
                .set_body_json(json!([{"id": "p1", "name": "Review", "ownerName": "alice"}])),
            status => ResponseTemplate::new(status),
        }
    }
}

async fn api(navidrome: &TokenNavidrome, http: reqwest::Client) -> (MockServer, NavidromePlaylistApi) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome.clone())
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    }));
    let identity = NavidromeIdentityService::new(
        settings.clone(),
        crate::services::http_client_factory::default_client(),
    );
    (server, NavidromePlaylistApi::new(http, identity, settings))
}

#[tokio::test]
async fn list_playlists_a_lapsed_token_logs_in_again_and_retries_once() {
    let navidrome = TokenNavidrome::new(|bearer| if bearer == "Bearer jwt-1" { 401 } else { 200 });
    let (_server, api) = api(&navidrome, crate::services::http_client_factory::default_client()).await;

    let playlists = api.list_playlists().await;

    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].name, "Review");
    assert_eq!(navidrome.logins.load(Ordering::SeqCst), 2);
    assert_eq!(*navidrome.bearers.lock(), ["Bearer jwt-1", "Bearer jwt-2"]);
}

#[tokio::test]
async fn list_playlists_a_fresh_token_refused_too_gives_up_after_one_retry() {
    let navidrome = TokenNavidrome::new(|_| 401);
    let (_server, api) = api(&navidrome, crate::services::http_client_factory::default_client()).await;

    assert!(api.list_playlists().await.is_empty());
    assert_eq!(navidrome.logins.load(Ordering::SeqCst), 2);
    assert_eq!(navidrome.bearers.lock().len(), 2);
}

#[tokio::test]
async fn add_tracks_sends_the_ids_navidrome_expects() {
    let navidrome = TokenNavidrome::new(|_| 200);
    let (_server, api) = api(&navidrome, crate::services::http_client_factory::default_client()).await;

    assert_eq!(api.add_tracks("p1", &["a".into(), "b".into()]).await, Ok(true));
    assert_eq!(*navidrome.bodies.lock(), [r#"{"ids":["a","b"]}"#]);
}

/// HttpClient reported its own timeout as a cancellation the caller never asked for.
#[tokio::test]
async fn list_songs_a_navidrome_that_times_out_is_no_answer() {
    let mut navidrome = TokenNavidrome::new(|_| 200);
    navidrome.song_delay = Duration::from_secs(5);
    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .expect("a client");
    let (_server, api) = api(&navidrome, impatient).await;

    assert!(api.list_songs(0, 1000).await.is_none());
}
