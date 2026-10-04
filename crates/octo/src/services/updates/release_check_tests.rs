//! `ReleaseCheckTests.cs` (all but the two `ReleaseVersion` theories, which are 2-B's), with
//! GitHub as a wiremock server, plus the `update/release.json` fixture round trip.

use super::*;
use chrono::TimeZone;
use octo_core::settings::AppSettings;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const RELEASES: &str = r####"
[
  { "tag_name": "desktop-v1.3.2", "name": "Octo for Windows and Linux 1.3.2", "draft": false, "prerelease": false, "body": "app", "html_url": "https://github.com/winters27/octo/releases/tag/desktop-v1.3.2", "published_at": "2026-10-03T10:42:57Z" },
  { "tag_name": "2026.10.05", "name": "2026.10.05", "draft": true, "prerelease": false, "body": "draft", "html_url": "", "published_at": null },
  { "tag_name": "2026.10.04", "name": "2026.10.04", "draft": false, "prerelease": true, "body": "pre", "html_url": "", "published_at": "2026-10-04T10:00:00Z" },
  { "tag_name": "android-v1.3.0", "name": "Octo for Android 1.3.0", "draft": false, "prerelease": false, "body": "app", "html_url": "", "published_at": "2026-10-03T11:20:07Z" },
  { "tag_name": "2026.10.02.1", "name": "", "draft": false, "prerelease": false, "body": "### Fix\n- one", "html_url": "https://github.com/winters27/octo/releases/tag/2026.10.02.1", "published_at": "2026-10-02T22:00:00Z" },
  { "tag_name": "2026.10.02", "name": "2026.10.02", "draft": false, "prerelease": false, "body": "two", "html_url": "https://github.com/winters27/octo/releases/tag/2026.10.02", "published_at": "2026-10-02T20:00:00Z" },
  { "tag_name": "2026.10.01", "name": "2026.10.01", "draft": false, "prerelease": false, "body": "one", "html_url": "https://github.com/winters27/octo/releases/tag/2026.10.01", "published_at": "2026-10-01T21:33:35Z" }
]
"####;

/// GitHub as the tests see it, a clock they move, and the cache file.
struct Fixture {
    _dir: tempfile::TempDir,
    cache_path: PathBuf,
    github: MockServer,
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cache_path = dir.path().join("update").join("release.json");
        Fixture {
            _dir: dir,
            cache_path,
            github: MockServer::start().await,
            now: Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap())),
        }
    }

    fn check(&self, running: &str) -> ReleaseCheck {
        self.check_with(running, UpdateSettings::default())
    }

    fn check_with(&self, running: &str, updates: UpdateSettings) -> ReleaseCheck {
        self.check_at(&self.github.uri(), running, updates)
    }

    fn check_at(&self, api_base: &str, running: &str, updates: UpdateSettings) -> ReleaseCheck {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            updates,
            ..Default::default()
        }));
        let now = self.now.clone();
        ReleaseCheck::with_parts(
            &self.cache_path,
            reqwest::Client::new(),
            api_base,
            settings,
            Clock::new(move || *now.lock()),
            running,
        )
    }

    fn now(&self) -> DateTime<Utc> {
        *self.now.lock()
    }

    fn advance(&self, by: TimeDelta) {
        *self.now.lock() += by;
    }

    /// Replaces GitHub's answer.
    async fn answer(&self, response: ResponseTemplate) {
        self.github.reset().await;
        Mock::given(method("GET"))
            .and(path("/repos/winters27/octo/releases"))
            .and(query_param("per_page", "30"))
            .respond_with(response)
            .mount(&self.github)
            .await;
    }

    async fn requests(&self) -> Vec<Request> {
        self.github.received_requests().await.unwrap_or_default()
    }
}

fn ok(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn tags(notes: &[ReleaseNote]) -> Vec<&str> {
    notes.iter().map(|n| n.tag.as_str()).collect()
}

#[tokio::test]
async fn an_older_server_is_told_which_releases_are_newer() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES).insert_header("etag", "\"abc\"")).await;

    let view = f.check("2026.10.01").check(false).await;

    assert!(view.update_available);
    assert_eq!(view.standing, "behind");
    let latest = view.latest.expect("a latest release");
    assert_eq!(latest.tag, "2026.10.02.1");
    // A release without a name is named for its tag.
    assert_eq!(latest.name, "2026.10.02.1");
    assert_eq!(tags(&view.newer), vec!["2026.10.02.1", "2026.10.02"]);
    assert!(view.error.is_none());
    assert_eq!(view.checked_utc, Some(f.now()));
}

#[tokio::test]
async fn apps_drafts_and_prereleases_never_count() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;

    let view = f.check("2026.10.02.1").check(false).await;

    assert!(!view.update_available);
    assert_eq!(view.standing, "current");
    assert!(view.newer.is_empty());
}

#[tokio::test]
async fn a_build_cut_after_the_newest_release_is_ahead() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;

    let view = f.check("2026.10.03").check(false).await;

    assert!(!view.update_available);
    assert_eq!(view.standing, "ahead");
}

#[tokio::test]
async fn a_build_that_names_no_release_is_never_told_it_is_behind() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;

    let view = f.check("1.0.0").check(false).await;

    assert!(!view.update_available);
    assert_eq!(view.standing, "unknown");
    assert_eq!(view.latest.expect("a latest release").tag, "2026.10.02.1");
}

#[tokio::test]
async fn it_asks_the_release_list_with_its_own_name() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;

    f.check("2026.10.01").check(false).await;

    let requests = f.requests().await;
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.url.path(), "/repos/winters27/octo/releases");
    assert_eq!(request.url.query(), Some("per_page=30"));
    assert_eq!(ReleaseCheck::GITHUB_API, "https://api.github.com");
    assert!(header(request, "user-agent").is_some_and(|ua| ua.starts_with("Octo/2026.10.01")));
    assert_eq!(
        header(request, "accept").as_deref(),
        Some("application/vnd.github+json")
    );
    assert_eq!(
        header(request, "x-github-api-version").as_deref(),
        Some("2022-11-28")
    );
    assert!(header(request, "if-none-match").is_none(), "nothing cached yet");
}

#[tokio::test]
async fn an_unchanged_list_costs_nothing_and_keeps_the_releases() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES).insert_header("etag", "\"abc\"")).await;
    let check = f.check("2026.10.01");
    check.check(false).await;

    f.advance(TimeDelta::hours(7));
    f.answer(ResponseTemplate::new(304)).await;
    let view = check.check(false).await;

    let requests = f.requests().await;
    assert_eq!(header(&requests[0], "if-none-match").as_deref(), Some("\"abc\""));
    assert_eq!(view.latest.expect("kept").tag, "2026.10.02.1");
    assert_eq!(view.checked_utc, Some(f.now()));
}

#[tokio::test]
async fn git_hubs_limit_keeps_the_last_answer_and_says_why() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;
    let check = f.check("2026.10.01");
    check.check(false).await;
    let checked_at = f.now();

    f.advance(TimeDelta::hours(7));
    f.answer(
        ResponseTemplate::new(403)
            .set_body_string(r#"{"message":"API rate limit exceeded"}"#)
            .insert_header("x-ratelimit-remaining", "0"),
    )
    .await;
    let view = check.check(false).await;

    assert!(view.error.as_deref().is_some_and(|e| e.contains("limit")));
    assert_eq!(view.latest.expect("kept").tag, "2026.10.02.1");
    assert!(view.update_available);
    assert_eq!(view.checked_utc, Some(checked_at));
}

#[tokio::test]
async fn no_network_keeps_the_last_answer_and_says_why() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;
    f.check("2026.10.01").check(false).await;

    // The same saved state, and GitHub out of reach (the C# handler threw HttpRequestException).
    f.advance(TimeDelta::hours(7));
    let offline = f.check_at("http://127.0.0.1:1", "2026.10.01", UpdateSettings::default());
    let view = offline.check(false).await;

    assert_eq!(
        view.error.as_deref(),
        Some("Couldn't reach GitHub to look for a new release.")
    );
    assert_eq!(view.latest.expect("kept").tag, "2026.10.02.1");
}

#[tokio::test]
async fn a_restart_answers_from_the_saved_check() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;
    f.check("2026.10.01").check(false).await;

    let view = f.check("2026.10.01").view();

    assert_eq!(f.requests().await.len(), 1);
    assert!(view.update_available);
    assert_eq!(view.latest.expect("saved").tag, "2026.10.02.1");
}

#[tokio::test]
async fn checks_off_means_git_hub_is_never_asked() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;
    let view = f
        .check_with(
            "2026.10.01",
            UpdateSettings {
                check: false,
                ..Default::default()
            },
        )
        .check(true)
        .await;

    assert!(f.requests().await.is_empty());
    assert!(!view.enabled);
    assert!(!view.update_available);
}

#[tokio::test]
async fn check_now_is_honoured_once_a_minute() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;
    let check = f.check("2026.10.01");

    check.check(true).await;
    check.check(true).await;
    assert_eq!(f.requests().await.len(), 1);

    f.advance(TimeDelta::minutes(1));
    check.check(true).await;
    assert_eq!(f.requests().await.len(), 2);
}

#[tokio::test]
async fn a_malformed_repo_is_refused_without_asking() {
    let f = Fixture::new().await;
    let view = f
        .check_with(
            "2026.10.01",
            UpdateSettings {
                repo: "not a repo".into(),
                ..Default::default()
            },
        )
        .check(false)
        .await;

    assert!(f.requests().await.is_empty());
    assert!(view.error.as_deref().is_some_and(|e| e.contains("owner/name")));
}

#[tokio::test]
async fn an_answer_about_another_repo_is_not_shown() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES)).await;
    f.check("2026.10.01").check(false).await;

    let view = f
        .check_with(
            "2026.10.01",
            UpdateSettings {
                repo: "someone/fork".into(),
                ..Default::default()
            },
        )
        .view();

    assert!(view.latest.is_none());
    assert!(!view.update_available);
}

#[tokio::test]
async fn answer_that_is_not_a_list_is_an_error() {
    let f = Fixture::new().await;
    f.answer(ok(r#"{"message":"odd"}"#)).await;

    let view = f.check("2026.10.01").check(false).await;

    assert!(view.error.is_some());
    assert!(view.latest.is_none());
}

// ---- Rust-only ------------------------------------------------------------------------

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/rust-migration/fixtures/state/update/release.json"
);

#[test]
fn the_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    let state: ReleaseCheckState = serde_json::from_str(&text).expect("the fixture reads");
    assert_eq!(state.e_tag.as_deref(), Some("W/\"5f2c8a0e1b\""));
    assert_eq!(octo_core::json::to_string(&state), text.trim_end_matches('\n'));
}

#[tokio::test]
async fn the_saved_state_is_what_the_fixture_shape_says() {
    let f = Fixture::new().await;
    f.answer(ok(RELEASES).insert_header("etag", "W/ \"abc\"")).await;
    f.check("2026.10.01").check(false).await;
    let saved = std::fs::read_to_string(&f.cache_path).expect("saved");
    assert!(saved.starts_with(
        r####"{"Repo":"winters27/octo","CheckedUtc":"2026-10-03T12:00:00Z","AttemptedUtc":"2026-10-03T12:00:00Z","Error":null,"ETag":"W/\u0022abc\u0022","Releases":[{"Tag":"2026.10.02.1","Name":"2026.10.02.1","Notes":"### Fix\n- one","####
    ), "{saved}");
    assert!(!state_file::tmp_path(&f.cache_path).exists());
}

#[test]
fn an_unreadable_cache_starts_fresh() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("release.json");
    std::fs::write(&path, "{\"Releases\":").expect("writes");
    let check = ReleaseCheck::new(&path, Arc::new(SettingsStore::for_tests(AppSettings::default())));
    let view = check.view();
    assert!(view.latest.is_none() && view.checked_utc.is_none() && view.error.is_none());
}

#[test]
fn entity_tags_parse_as_dotnet_did() {
    for (text, expected) in [
        ("\"abc\"", Some("\"abc\"")),
        ("W/\"5f2c\"", Some("W/\"5f2c\"")),
        ("abc", None),
        ("\"a b\"", Some("\"a b\"")),
        ("W/ \"x\"", Some("W/\"x\"")),
        (" \"x\" ", Some("\"x\"")),
    ] {
        assert_eq!(parse_entity_tag(text).as_deref(), expected, "{text}");
    }
}

#[test]
fn releases_keep_ten_newest_and_cut_long_notes() {
    let many: Vec<Value> = (1..=12)
        .map(|day| {
            serde_json::json!({
                "tag_name": format!("2026.09.{day:02}"),
                "body": if day == 12 { "x".repeat(20_005) } else { String::new() },
                "published_at": "2026-09-01T00:00:00+02:00",
            })
        })
        .collect();
    let notes = ReleaseCheck::server_releases(&Value::Array(many)).expect("a list");
    assert_eq!(notes.len(), 10);
    assert_eq!(notes[0].tag, "2026.09.12");
    assert_eq!(notes[0].notes.len(), 20_000);
    assert_eq!(notes[9].tag, "2026.09.03");
    assert_eq!(
        notes[0].published_utc,
        Some(Utc.with_ymd_and_hms(2026, 8, 31, 22, 0, 0).unwrap())
    );
    assert_eq!(notes[0].url, "");
    let plus = serde_json::json!([{ "tag_name": "2026.10.02+abc" }]);
    assert!(ReleaseCheck::server_releases(&plus).expect("a list").is_empty());
    assert_eq!(ReleaseCheck::sanitize("2026.10.02 (dev)"), "2026.10.02dev");
    assert_eq!(ReleaseCheck::sanitize("+++"), "unknown");
}

#[tokio::test(start_paused = true)]
async fn the_worker_waits_then_checks_when_due_and_stops_on_shutdown() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("release.json");
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap()));
    let clock = {
        let now = now.clone();
        Clock::new(move || *now.lock())
    };
    // A repo that is refused without asking still records the attempt, and keeps the network
    // (and real time) out of a test on virtual time.
    let settings = AppSettings {
        updates: UpdateSettings {
            repo: "not a repo".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let check = Arc::new(ReleaseCheck::with_parts(
        &path,
        reqwest::Client::new(),
        "http://127.0.0.1:1",
        Arc::new(SettingsStore::for_tests(settings)),
        clock,
        "2026.10.01",
    ));
    let attempted = |check: &ReleaseCheck| check.state.lock().attempted_utc;
    let token = CancellationToken::new();
    let worker = tokio::spawn(check.clone().run(token.clone()));

    tokio::time::sleep(Duration::from_secs(19)).await;
    assert_eq!(attempted(&check), None, "not before 20 s");
    let first = *now.lock();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(attempted(&check), Some(first));
    assert!(path.exists(), "the attempt is saved");

    *now.lock() += TimeDelta::hours(1);
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    assert_eq!(attempted(&check), Some(first), "not due for 6 hours");

    *now.lock() += TimeDelta::hours(6);
    let second = *now.lock();
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    assert_eq!(attempted(&check), Some(second));

    token.cancel();
    worker.await.expect("joins").expect("ok");
}
