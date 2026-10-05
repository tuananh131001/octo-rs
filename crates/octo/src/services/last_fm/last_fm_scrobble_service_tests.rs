//! `LastFmScrobbleTests.cs` (`LastFmScrobbleServiceTests`).
//!
//! Outside songs reach Last.fm because Navidrome never hears of them. Library songs must not,
//! because Navidrome scrobbles those itself and a second copy would count every play twice.
//! Everything here talks to [`FakeLastFm`]; no test reaches the real Last.fm.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use octo_core::common::Clock;
use octo_core::json::dom::Node;
use octo_core::last_fm::LastFmTrack;
use octo_core::last_fm::last_fm_scrobble_service::sign;
use octo_core::settings::{
    AppSettings, LastFmSettings, LastFmUserSession, SettingsFileWriter, SettingsStore,
};
use parking_lot::Mutex;
use tokio::sync::{oneshot, watch};

use super::{LastFmScrobbleService, ScrobbleTime, ScrobbleTuning};

const API_KEY: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &str = "s3cr3t";

type Call = IndexMap<String, String>;
type Hold = Arc<dyn Fn(usize) -> BoxFuture<'static, ()> + Send + Sync>;

/// Last.fm's web service as far as Octo uses it. Refuses a wrong signature with error 13 as the
/// real one does, so every call a test sees was signed correctly.
#[derive(Default)]
struct FakeLastFm {
    calls: Mutex<Vec<(Call, DateTime<Utc>)>>,
    /// What the next calls fail with, in order: a Last.fm error code, or 0 for HTTP 503.
    failures: Mutex<VecDeque<i32>>,
    /// Whether the admin has approved Octo on last.fm yet.
    approved: AtomicBool,
    /// Holds a call open until the returned future completes; given how many calls arrived.
    hold: Mutex<Option<Hold>>,
}

impl FakeLastFm {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().iter().map(|(call, _)| call.clone()).collect()
    }

    /// When each call arrived, on the real clock.
    fn call_times(&self) -> Vec<DateTime<Utc>> {
        self.calls.lock().iter().map(|(_, at)| *at).collect()
    }

    fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.get("method").map(String::as_str) == Some(method))
            .collect()
    }

    fn fail_next(&self, code: i32) {
        self.failures.lock().push_back(code);
    }

    fn set_hold(&self, hold: Option<Hold>) {
        *self.hold.lock() = hold;
    }

    /// Starts the fake on a local port; answers at `<base>/2.0/`.
    async fn serve(self: &Arc<Self>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let address = listener.local_addr().expect("an address");
        let app = Router::new().fallback(respond).with_state(Arc::clone(self));
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{address}/2.0/")
    }
}

async fn respond(State(fake): State<Arc<FakeLastFm>>, body: String) -> Response {
    let call: Call = url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let count = {
        let mut calls = fake.calls.lock();
        calls.push((call.clone(), Utc::now()));
        calls.len()
    };
    let hold = fake.hold.lock().clone();
    if let Some(hold) = hold {
        hold(count).await;
    }

    let signature = sign(call.iter().map(|(k, v)| (k.as_str(), v.as_str())), SECRET);
    if call.get("api_sig") != Some(&signature) {
        return error(13, "Invalid method signature supplied");
    }
    let failure = fake.failures.lock().pop_front();
    if let Some(failure) = failure {
        return if failure == 0 {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        } else {
            error(failure, "Refused by the fixture")
        };
    }

    match call.get("method").map(String::as_str) {
        Some("auth.getToken") => ok(r#"{"token":"tok-1"}"#.to_string()),
        Some("auth.getSession") if fake.approved.load(Ordering::SeqCst) => {
            ok(r#"{"session":{"name":"lfm-alice","key":"sk-alice","subscriber":0}}"#.to_string())
        }
        Some("auth.getSession") => error(14, "Unauthorized Token - This token has not been authorized"),
        Some("track.scrobble") => {
            let accepted = call.keys().filter(|key| key.starts_with("artist[")).count();
            ok(format!(
                r#"{{"scrobbles":{{"@attr":{{"accepted":{accepted},"ignored":0}},"scrobble":[]}}}}"#
            ))
        }
        Some("track.updateNowPlaying") => {
            ok(r##"{"nowplaying":{"ignoredMessage":{"code":"0","#text":""}}}"##.to_string())
        }
        _ => error(3, "Invalid Method - No method with that name in this package"),
    }
}

fn ok(json: String) -> Response {
    (StatusCode::OK, [("content-type", "application/json")], json).into_response()
}

// Last.fm answers a refusal with a 4xx and the error in the body.
fn error(code: i32, message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [("content-type", "application/json")],
        serde_json::json!({ "error": code, "message": message }).to_string(),
    )
        .into_response()
}

/// A gate a held call waits on until the test opens it (a `TaskCompletionSource`).
#[derive(Clone)]
struct Release(Arc<watch::Sender<bool>>);

impl Release {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    fn set(&self) {
        self.0.send_replace(true);
    }

    fn wait(&self) -> BoxFuture<'static, ()> {
        let mut receiver = self.0.subscribe();
        Box::pin(async move {
            let _ = receiver.wait_for(|open| *open).await;
        })
    }
}

/// A clock that moves only when told, with the timers the service's waits set on it.
struct ManualClock {
    state: Mutex<(DateTime<Utc>, Vec<(DateTime<Utc>, oneshot::Sender<()>)>)>,
}

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((Utc::now(), Vec::new())),
        })
    }

    fn time(self: &Arc<Self>) -> ScrobbleTime {
        let now = Arc::clone(self);
        let timers = Arc::clone(self);
        ScrobbleTime {
            clock: Clock::new(move || now.state.lock().0),
            sleep: Arc::new(move |wait| {
                let (tx, rx) = oneshot::channel();
                let mut state = timers.state.lock();
                let due = state.0 + TimeDelta::from_std(wait).expect("a short wait");
                state.1.push((due, tx));
                Box::pin(async move {
                    let _ = rx.await;
                })
            }),
        }
    }

    /// Timers set and not yet fired (a wait given up on is gone).
    fn waiting(&self) -> usize {
        let mut state = self.state.lock();
        state.1.retain(|(_, tx)| !tx.is_closed());
        state.1.len()
    }

    fn advance(&self, by: TimeDelta) {
        let mut state = self.state.lock();
        state.0 += by;
        let now = state.0;
        let (due, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.1)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        state.1 = kept;
        for (_, tx) in due {
            let _ = tx.send(());
        }
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    last_fm: Arc<FakeLastFm>,
    settings: Arc<SettingsStore>,
    file: Arc<SettingsFileWriter>,
    service: Arc<LastFmScrobbleService>,
}

fn session(key: &str, user: &str) -> LastFmUserSession {
    LastFmUserSession {
        session_key: key.to_string(),
        last_fm_user: user.to_string(),
    }
}

fn alice_settings() -> LastFmSettings {
    LastFmSettings {
        api_key: API_KEY.to_string(),
        api_secret: SECRET.to_string(),
        user_sessions: [("alice".to_string(), session("sk-alice", "lfm-alice"))]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

impl Fixture {
    async fn new() -> Self {
        Self::with(TimeDelta::hours(1), ScrobbleTime::system()).await
    }

    async fn with(refusal_grace: TimeDelta, time: ScrobbleTime) -> Self {
        let directory = tempfile::tempdir().expect("a temp dir");
        let file = Arc::new(SettingsFileWriter::new(directory.path().join("settings.json")));
        let Ok(Node::Object(initial)) = Node::parse(
            r#"{ "LastFm": { "UserSessions": { "alice": { "SessionKey": "sk-alice", "LastFmUser": "lfm-alice" } } } }"#,
        ) else {
            panic!("the initial settings parse");
        };
        file.merge(&initial, &[]).expect("the initial settings save");
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            last_fm: alice_settings(),
            ..Default::default()
        }));
        let last_fm = Arc::new(FakeLastFm::default());
        let api_url = last_fm.serve().await;
        let service = Arc::new(LastFmScrobbleService::with_parts(
            reqwest::Client::new(),
            &api_url,
            Arc::clone(&settings),
            Arc::clone(&file),
            ScrobbleTuning {
                retry_delay: TimeDelta::milliseconds(20),
                rate_limit_pause: TimeDelta::milliseconds(300),
                refusal_grace,
            },
            time,
        ));
        Self {
            _directory: directory,
            last_fm,
            settings,
            file,
            service,
        }
    }

    fn set_last_fm(&self, last_fm: LastFmSettings) {
        self.settings.set(AppSettings {
            last_fm,
            ..Default::default()
        });
    }

    async fn when_idle(&self) {
        let service = Arc::clone(&self.service);
        until(move || service.outstanding() == 0).await;
    }

    /// The session key settings.json holds for this user.
    fn saved_session(&self, user: &str) -> Option<String> {
        self.file
            .load()
            .get("LastFm")
            .and_then(|section| section.get("UserSessions"))
            .and_then(|sessions| sessions.get(user))
            .and_then(|entry| entry.get("SessionKey"))
            .and_then(Node::as_str)
            .map(str::to_string)
    }

    fn notice(&self, user: &str) -> Option<String> {
        self.service
            .users(Vec::<String>::new())
            .into_iter()
            .find(|row| row.user == user)
            .and_then(|row| row.notice)
    }

    fn connected(&self, user: &str) -> bool {
        self.service
            .users(Vec::<String>::new())
            .into_iter()
            .find(|row| row.user == user)
            .expect("the user is listed")
            .connected
    }
}

fn song() -> LastFmTrack {
    LastFmTrack::new("Bladee", "Be Nice 2 Me", Some("Icedancer"), Some(154))
}

fn titled(title: &str) -> LastFmTrack {
    LastFmTrack {
        title: title.to_string(),
        ..song()
    }
}

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(condition(), "the condition never held");
}

fn sig_of(call: &Call) -> String {
    sign(call.iter().map(|(k, v)| (k.as_str(), v.as_str())), SECRET)
}

fn field<'a>(call: &'a Call, name: &str) -> Option<&'a str> {
    call.get(name).map(String::as_str)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_play_is_scrobbled_with_everything_last_fm_asks_for() {
    let f = Fixture::new().await;
    let played_at = (Utc::now() - TimeDelta::hours(1)).timestamp();
    f.service.scrobble(
        "Alice",
        song(),
        DateTime::from_timestamp(played_at, 0).expect("a time"),
        true,
    );
    f.when_idle().await;

    let calls = f.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(field(call, "sk"), Some("sk-alice"));
    assert_eq!(field(call, "api_key"), Some(API_KEY));
    assert_eq!(field(call, "artist[0]"), Some("Bladee"));
    assert_eq!(field(call, "track[0]"), Some("Be Nice 2 Me"));
    assert_eq!(field(call, "album[0]"), Some("Icedancer"));
    assert_eq!(field(call, "duration[0]"), Some("154"));
    assert_eq!(field(call, "timestamp[0]"), Some(played_at.to_string().as_str()));
    assert!(!call.contains_key("chosenByUser[0]"));
    assert_eq!(field(call, "api_sig"), Some(sig_of(call).as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_tracks_and_users_without_a_session_send_nothing() {
    let f = Fixture::new().await;
    f.service.scrobble(
        "alice",
        LastFmTrack {
            duration_seconds: Some(29),
            ..song()
        },
        Utc::now(),
        true,
    );
    f.service.scrobble("bob", song(), Utc::now(), true);
    f.service.now_playing("bob", song());
    f.when_idle().await;

    assert!(f.last_fm.calls().is_empty());
}

/// Plays that pile up while one call is out go together, at most 50 a call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_plays_go_in_batches_of_fifty() {
    let f = Fixture::new().await;
    let release = Release::new();
    let held = release.clone();
    f.last_fm.set_hold(Some(Arc::new(move |_| held.wait())));
    f.service.scrobble("alice", song(), Utc::now(), true);
    let fake = Arc::clone(&f.last_fm);
    until(move || fake.calls().len() == 1).await;
    for i in 0..60 {
        f.service.scrobble(
            "alice",
            titled(&format!("Song {i}")),
            Utc::now() - TimeDelta::minutes(i),
            true,
        );
    }
    f.last_fm.set_hold(None);
    release.set();
    f.when_idle().await;

    let sizes: Vec<usize> = f
        .last_fm
        .calls_to("track.scrobble")
        .iter()
        .map(|call| call.keys().filter(|key| key.starts_with("artist[")).count())
        .collect();
    assert_eq!(sizes, [1, 50, 10]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_last_fm_is_retried() {
    let f = Fixture::new().await;
    f.last_fm.fail_next(0);
    f.last_fm.fail_next(16);

    f.service.scrobble("alice", song(), Utc::now(), true);
    f.when_idle().await;

    let calls = f.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 3);
    let mut stamps: Vec<&str> = calls
        .iter()
        .filter_map(|call| field(call, "timestamp[0]"))
        .collect();
    stamps.dedup();
    assert_eq!(stamps.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_are_bounded() {
    let f = Fixture::new().await;
    for _ in 0..10 {
        f.last_fm.fail_next(0);
    }

    f.service.scrobble("alice", song(), Utc::now(), true);
    f.when_idle().await;

    assert_eq!(
        f.last_fm.calls_to("track.scrobble").len(),
        LastFmScrobbleService::MAX_ATTEMPTS as usize
    );
}

/// Error 29: nothing more goes until the pause is over, then the play still does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_pauses_then_sends() {
    // The pause runs on a clock the test moves, so a slow machine cannot end it early.
    let clock = ManualClock::new();
    let f = Fixture::with(TimeDelta::hours(1), clock.time()).await;
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_RATE_LIMITED);

    f.service.scrobble("alice", song(), Utc::now(), true);
    // Refused, and the queue now waits out the pause.
    let watched = Arc::clone(&clock);
    until(move || watched.waiting() == 1).await;
    // A play and a Now Playing arriving meanwhile wait too.
    f.service.scrobble("alice", titled("Song 2"), Utc::now(), true);
    f.service.now_playing("alice", song());
    let service = Arc::clone(&f.service);
    until(move || service.outstanding() == 2).await;
    let watched = Arc::clone(&clock);
    until(move || watched.waiting() == 1).await;
    let pause = f.service.tuning().rate_limit_pause;
    clock.advance(pause - TimeDelta::nanoseconds(100));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(clock.waiting(), 1);
    assert_eq!(f.last_fm.calls().len(), 1);

    clock.advance(TimeDelta::nanoseconds(100));
    f.when_idle().await;

    let calls = f.last_fm.calls();
    let methods: Vec<&str> = calls.iter().filter_map(|call| field(call, "method")).collect();
    assert_eq!(methods, ["track.scrobble", "track.scrobble"]);
    // The refused play went again, with the one that waited behind it.
    assert_eq!(
        (field(&calls[1], "track[0]"), field(&calls[1], "track[1]")),
        (Some("Be Nice 2 Me"), Some("Song 2"))
    );
}

/// Error 9 is Last.fm saying the listener revoked Octo. The session stops at once and
/// the dashboard says why, but one refusal is not enough to delete what the admin saved, nor
/// the plays: the refused one and any new ones wait out the grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_session_pauses_the_user_and_keeps_the_saved_session() {
    let f = Fixture::new().await;
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);

    f.service.scrobble("alice", song(), Utc::now(), true);
    until(|| f.notice("alice").is_some()).await;

    assert_eq!(f.saved_session("alice").as_deref(), Some("sk-alice"));
    assert!(!f.connected("alice"));
    assert!(f.notice("alice").expect("a notice").contains("Connect again"));

    f.service.scrobble("alice", titled("Later"), Utc::now(), true);
    f.service.now_playing("alice", song());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.last_fm.calls().len(), 1);
    assert_eq!(f.service.outstanding(), 2);
}

/// After the grace the session is tried once more. Refused again, it is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_session_twice_an_hour_apart_removes_the_saved_session() {
    let f = Fixture::with(TimeDelta::milliseconds(150), ScrobbleTime::system()).await;
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);
    let second = Release::new();
    let held = second.clone();
    f.last_fm.set_hold(Some(Arc::new(move |count| {
        if count > 1 {
            held.wait()
        } else {
            Box::pin(async {})
        }
    })));
    f.service.scrobble("alice", song(), Utc::now(), true);

    // The kept play is what tries the session again, once the grace is over.
    let fake = Arc::clone(&f.last_fm);
    until(move || fake.calls().len() == 2).await;
    assert_eq!(f.saved_session("alice").as_deref(), Some("sk-alice"));
    second.set();
    f.when_idle().await;

    assert_eq!(f.last_fm.calls_to("track.scrobble").len(), 2);
    let times = f.last_fm.call_times();
    assert!(times[1] - times[0] >= f.service.tuning().refusal_grace);
    assert_eq!(f.saved_session("alice"), None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!f.service.is_enabled_for("alice"));
    f.service.scrobble("alice", song(), Utc::now(), true);
    assert_eq!(f.service.outstanding(), 0);
    assert!(f.notice("alice").expect("a notice").contains("Connect again"));
}

/// A session Last.fm takes again after the grace was never revoked: the notice goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_session_then_accepted_is_connected_again() {
    let f = Fixture::with(TimeDelta::milliseconds(150), ScrobbleTime::system()).await;
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);
    f.service.scrobble("alice", song(), Utc::now(), true);
    f.when_idle().await;

    // The play refused with the session was kept, and went once the grace was over.
    let calls = f.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 2);
    assert_eq!(field(&calls[0], "timestamp[0]"), field(&calls[1], "timestamp[0]"));
    assert_eq!(f.saved_session("alice").as_deref(), Some("sk-alice"));
    assert!(f.connected("alice"));
    assert_eq!(f.notice("alice"), None);
}

/// Error 8 ("operation failed") comes back the same however often it is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_failed_is_not_retried() {
    let f = Fixture::new().await;
    f.last_fm.fail_next(8);

    f.service.scrobble("alice", song(), Utc::now(), true);
    f.when_idle().await;

    assert_eq!(f.last_fm.calls_to("track.scrobble").len(), 1);
}

/// Last.fm ignores plays older than two weeks, so they are not sent to be ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plays_older_than_two_weeks_are_not_queued() {
    let f = Fixture::new().await;
    f.service
        .scrobble("alice", song(), Utc::now() - TimeDelta::days(15), true);
    f.service
        .scrobble("alice", titled("Recent"), Utc::now() - TimeDelta::days(13), true);
    f.when_idle().await;

    let calls = f.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 1);
    assert_eq!(field(&calls[0], "track[0]"), Some("Recent"));
    assert!(!calls[0].contains_key("track[1]"));
}

/// A radio stream picks the next song itself; Last.fm is told the listener did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_play_the_listener_did_not_pick_is_sent_as_not_chosen() {
    let f = Fixture::new().await;
    let release = Release::new();
    let held = release.clone();
    f.last_fm.set_hold(Some(Arc::new(move |_| held.wait())));
    f.service
        .scrobble("alice", song(), Utc::now() - TimeDelta::minutes(9), true);
    let fake = Arc::clone(&f.last_fm);
    until(move || fake.calls().len() == 1).await;
    f.service.scrobble(
        "alice",
        titled("Picked"),
        Utc::now() - TimeDelta::minutes(6),
        true,
    );
    f.service.scrobble(
        "alice",
        titled("Radio"),
        Utc::now() - TimeDelta::minutes(3),
        false,
    );
    f.last_fm.set_hold(None);
    release.set();
    f.when_idle().await;

    let batch = f.last_fm.calls_to("track.scrobble").pop().expect("a batch");
    assert_eq!(field(&batch, "track[0]"), Some("Picked"));
    assert!(!batch.contains_key("chosenByUser[0]"));
    assert_eq!(field(&batch, "track[1]"), Some("Radio"));
    assert_eq!(field(&batch, "chosenByUser[1]"), Some("0"));
    assert_eq!(field(&batch, "api_sig"), Some(sig_of(&batch).as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_session_on_now_playing_also_disconnects() {
    let f = Fixture::new().await;
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);

    f.service.now_playing("alice", song());
    f.when_idle().await;

    let calls = f.last_fm.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(field(&calls[0], "method"), Some("track.updateNowPlaying"));
    assert!(!f.connected("alice"));
}

/// A refusal by Now Playing rests the session too: a play finished meanwhile waits
/// for the grace rather than going with it, and goes after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_session_on_now_playing_plays_wait_out_the_grace() {
    let f = Fixture::with(TimeDelta::milliseconds(150), ScrobbleTime::system()).await;
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);
    f.service.now_playing("alice", song());
    f.when_idle().await;

    f.service.scrobble("alice", song(), Utc::now(), true);
    f.when_idle().await;

    let times = f.last_fm.call_times();
    assert_eq!(times.len(), 2);
    assert_eq!(f.last_fm.calls_to("track.scrobble").len(), 1);
    assert!(times[1] - times[0] >= f.service.tuning().refusal_grace);
}

/// One listener's plays waiting out a refusal do not hold back another's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resting_listener_does_not_hold_back_another() {
    let f = Fixture::new().await;
    f.set_last_fm(LastFmSettings {
        user_sessions: [
            ("alice".to_string(), session("sk-alice", "lfm-alice")),
            ("bob".to_string(), session("sk-bob", "lfm-bob")),
        ]
        .into_iter()
        .collect(),
        ..alice_settings()
    });
    f.last_fm.fail_next(LastFmScrobbleService::ERROR_INVALID_SESSION);
    f.service.scrobble("alice", song(), Utc::now(), true);
    until(|| f.last_fm.calls().len() == 1 && f.notice("alice").is_some()).await;

    f.service.scrobble("bob", song(), Utc::now(), true);
    let service = Arc::clone(&f.service);
    until(move || service.outstanding() == 1).await;

    let keys: Vec<String> = f
        .last_fm
        .calls_to("track.scrobble")
        .iter()
        .filter_map(|call| call.get("sk").cloned())
        .collect();
    assert_eq!(keys, ["sk-alice", "sk-bob"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switched_off_sends_nothing() {
    let f = Fixture::new().await;
    f.set_last_fm(LastFmSettings {
        scrobble_external_plays: false,
        ..alice_settings()
    });

    f.service.scrobble("alice", song(), Utc::now(), true);
    f.service.now_playing("alice", song());
    f.when_idle().await;

    assert!(f.last_fm.calls().is_empty());
}

// ---- The dashboard's Connect flow -----------------------------------------------------------
// The settings store here never reloads from the file, which is the moment right after Finish
// on a real server: settings.json has the session, the running settings do not yet.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_shows_the_listener_connected_before_the_settings_reload() {
    let f = Fixture::new().await;
    f.service.begin_connect("bob").await.expect("a link");
    f.last_fm.approved.store(true, Ordering::SeqCst);
    f.service.finish_connect("bob").await.expect("connected");

    let bob = f
        .service
        .users(Vec::<String>::new())
        .into_iter()
        .find(|row| row.user == "bob")
        .expect("bob is listed");
    assert!(bob.connected);
    assert_eq!(bob.last_fm_user.as_deref(), Some("lfm-alice"));
    assert!(f.service.is_enabled_for("bob"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_right_after_finish_is_not_connected() {
    let f = Fixture::new().await;
    f.service.begin_connect("bob").await.expect("a link");
    f.last_fm.approved.store(true, Ordering::SeqCst);
    f.service.finish_connect("bob").await.expect("connected");

    f.service.disconnect("bob").expect("disconnected");

    assert!(
        !f.service
            .users(["bob"])
            .iter()
            .any(|row| row.connected && row.user == "bob")
    );
    assert!(!f.service.is_enabled_for("bob"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_connect_carries_its_approval_link_until_cancelled() {
    let f = Fixture::new().await;
    let url = f.service.begin_connect("bob").await.expect("a link");

    let bob = f
        .service
        .users(Vec::<String>::new())
        .into_iter()
        .find(|row| row.user == "bob")
        .expect("bob is listed");
    assert!(bob.awaiting_approval);
    assert_eq!(bob.approval_url.as_deref(), Some(url.as_str()));

    f.service.cancel_connect("bob");

    assert!(
        !f.service
            .users(Vec::<String>::new())
            .iter()
            .any(|row| row.user == "bob")
    );
    assert!(f.service.finish_connect("bob").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_sent_is_the_play_last_fm_took() {
    let f = Fixture::new().await;
    let played_at = Utc::now() - TimeDelta::minutes(3);
    f.service.scrobble("alice", song(), played_at, true);
    f.when_idle().await;

    let sent = f
        .service
        .users(Vec::<String>::new())
        .into_iter()
        .find(|row| row.user == "alice")
        .and_then(|row| row.last_sent)
        .expect("a play was sent");
    assert_eq!(
        (sent.artist.as_str(), sent.title.as_str()),
        ("Bladee", "Be Nice 2 Me")
    );
    assert!((sent.played_at_utc - played_at).abs() <= TimeDelta::seconds(1));
}

// ---- Checking a key and secret before they are saved -----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_the_saved_pair_is_ok() {
    let f = Fixture::new().await;
    let check = f.service.check_credentials(None, None).await;

    assert_eq!((check.key.as_str(), check.secret.as_str()), ("ok", "ok"));
    let calls = f.last_fm.calls_to("auth.getSession");
    assert_eq!(calls.len(), 1);
    assert!(!f.last_fm.approved.load(Ordering::SeqCst));
    assert_eq!(field(&calls[0], "api_key"), Some(API_KEY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_a_secret_from_another_app_is_invalid() {
    let f = Fixture::new().await;
    let check = f.service.check_credentials(None, Some("not-the-secret")).await;

    assert_eq!((check.key.as_str(), check.secret.as_str()), ("ok", "invalid"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_the_key_pasted_as_the_secret_says_so() {
    let f = Fixture::new().await;
    let check = f
        .service
        .check_credentials(Some(API_KEY), Some(&API_KEY.to_uppercase()))
        .await;

    assert_eq!((check.key.as_str(), check.secret.as_str()), ("ok", "same-as-key"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_a_key_last_fm_does_not_know_is_invalid() {
    let f = Fixture::new().await;
    f.last_fm.fail_next(10);

    let check = f
        .service
        .check_credentials(Some("ffffffffffffffffffffffffffffffff"), None)
        .await;

    assert_eq!(
        (check.key.as_str(), check.secret.as_str()),
        ("invalid", "unchecked")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_last_fm_down_is_unreachable_not_invalid() {
    let f = Fixture::new().await;
    f.last_fm.fail_next(0);

    let check = f.service.check_credentials(None, None).await;

    assert_eq!(
        (check.key.as_str(), check.secret.as_str()),
        ("unreachable", "unchecked")
    );
    assert_eq!(
        check.message.as_deref(),
        Some("Last.fm could not be reached: HTTP 503")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_no_key_at_all_is_missing_and_asks_nothing() {
    let f = Fixture::new().await;
    f.set_last_fm(LastFmSettings::default());

    let check = f.service.check_credentials(None, None).await;

    assert_eq!(
        (check.key.as_str(), check.secret.as_str()),
        ("missing", "missing")
    );
    assert!(f.last_fm.calls().is_empty());
}

/// Rust-only: what Finish writes to settings.json and what it says when it cannot finish, and
/// a disconnect of a session the file does not hold (one set in the environment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_writes_through_the_settings_file_and_says_why_it_cannot() {
    let f = Fixture::new().await;
    assert_eq!(
        f.service
            .finish_connect("bob")
            .await
            .expect_err("no Connect")
            .to_string(),
        "Start with Connect. An approval link lasts an hour."
    );
    assert_eq!(
        f.service
            .begin_connect(" a:b ")
            .await
            .expect_err("a colon")
            .to_string(),
        "A Navidrome username is required."
    );

    let url = f.service.begin_connect(" Bob ").await.expect("a link");
    assert_eq!(
        url,
        format!("https://www.last.fm/api/auth/?api_key={API_KEY}&token=tok-1")
    );
    let early = f
        .service
        .finish_connect("bob")
        .await
        .expect_err("not approved yet");
    match early {
        super::LastFmConnectError::Refused(refusal) => {
            assert_eq!(refusal.code, LastFmScrobbleService::ERROR_TOKEN_NOT_AUTHORIZED);
            assert_eq!(
                refusal.message,
                "Last.fm has not seen the approval yet. Open the link, allow access, then Finish."
            );
        }
        other => panic!("{other:?}"),
    }

    f.last_fm.approved.store(true, Ordering::SeqCst);
    f.service.finish_connect("BOB").await.expect("connected");
    // Saved under the name as Finish was given it; the call carried the token Connect got.
    assert_eq!(f.saved_session("BOB").as_deref(), Some("sk-alice"));
    assert_eq!(
        f.last_fm
            .calls_to("auth.getSession")
            .last()
            .and_then(|call| call.get("token").cloned()),
        Some("tok-1".to_string())
    );

    // A reconnect under another spelling replaces the entry rather than adding one.
    f.service.begin_connect("bob").await.expect("a link");
    f.service.finish_connect("bob").await.expect("connected");
    let names: Vec<String> = f
        .file
        .load()
        .get("LastFm")
        .and_then(|section| section.get("UserSessions"))
        .and_then(Node::as_object)
        .expect("sessions")
        .keys()
        .cloned()
        .collect();
    assert_eq!(names, ["alice", "bob"]);

    f.set_last_fm(LastFmSettings {
        user_sessions: [("carol".to_string(), session("sk-carol", "lfm-carol"))]
            .into_iter()
            .collect(),
        ..alice_settings()
    });
    assert!(f.service.is_enabled_for("carol"));
    assert!(!f.service.disconnect("carol").expect("disconnected"));
    assert!(!f.service.is_enabled_for("carol"));
    assert!(f.service.disconnect("bob").expect("disconnected"));
    assert_eq!(f.saved_session("bob"), None);
}
