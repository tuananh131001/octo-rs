//! The SoulseekClient tests that talk to a fake slskd: SoulseekSearchProfileTests (all but the
//! payload half, which is in `octo_core::soulseek::soulseek_client`), SoulseekSlowTransferTests'
//! waits and cancels, and ParallelDownloadTests' enqueue tests. The C# fakes were
//! `HttpMessageHandler`s; here they answer from a wiremock server the client is pointed at.

use std::sync::Arc;

use chrono::TimeZone;
use octo_core::settings::SoulseekSettings;
use parking_lot::Mutex;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 2, 0, 0, 0)
        .single()
        .expect("a date")
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn session() -> ResponseTemplate {
    json(&format!(
        r#"{{"token":"jwt","expires":{}}}"#,
        (Utc::now() + TimeDelta::hours(1)).timestamp()
    ))
}

async fn serve(responder: impl Respond + 'static) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(responder).mount(&server).await;
    server
}

fn settings(server: &MockServer) -> SoulseekSettings {
    SoulseekSettings {
        base_url: Some(format!("{}/", server.uri())),
        username: Some("octo".into()),
        password: Some("secret".into()),
        ..Default::default()
    }
}

fn interactive() -> SearchProfile {
    SearchProfile::interactive(&SoulseekSettings::default())
}

// ---- SoulseekSearchProfileTests -------------------------------------------------------------
//
// slskd hands over a search's answers only when the search ends (#70). These pin how Octo waits
// for that, cancels a search still running at its ceiling so the answers are kept, and never
// leaves a search running in slskd behind it.

const ANSWERS: &str = r#"[{"username":"peer","uploadSpeed":1000000,"queueLength":0,"files":[
  {"filename":"Music\\Artist\\01 - Song.flac","size":30000000,"extension":"flac","length":200}]}]"#;

#[derive(Default)]
struct SearchState {
    reads: i32,
    cancel_asked: bool,
    saved: bool,
    state: String,
    now: Option<DateTime<Utc>>,
    posted: Vec<String>,
    calls: Vec<String>,
}

/// slskd's search API as 0.26.0 behaves: answers are saved only when a search ends, the record
/// says Completed one look before they are, PUT cancels, DELETE only removes the record. The
/// clock moves half a second with every look at the state.
#[derive(Clone)]
struct FakeSearchSlskd {
    ends_after: Option<i32>,
    ignores_cancel: bool,
    refuse_first_start: bool,
    on_status_read: Option<Arc<dyn Fn(i32) + Send + Sync>>,
    on_start_sent: Option<Arc<dyn Fn() + Send + Sync>>,
    state: Arc<Mutex<SearchState>>,
}

impl FakeSearchSlskd {
    fn new(ends_after: Option<i32>) -> Self {
        FakeSearchSlskd {
            ends_after,
            ignores_cancel: false,
            refuse_first_start: false,
            on_status_read: None,
            on_start_sent: None,
            state: Arc::new(Mutex::new(SearchState {
                state: "InProgress".into(),
                now: Some(start()),
                ..Default::default()
            })),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.state.lock().now.expect("set")
    }

    fn posted(&self) -> Vec<String> {
        self.state.lock().posted.clone()
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().calls.clone()
    }

    fn cancels_and_deletes(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c == "PUT" || c == "DELETE")
            .collect()
    }
}

impl Respond for FakeSearchSlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        let body = String::from_utf8_lossy(&request.body).to_string();
        if let Some(sent) = &self.on_start_sent
            && request.method == Method::POST
            && path == "/api/v0/searches"
        {
            // slskd takes the search, and the answer is held until the caller gives up.
            self.state.lock().posted.push(body);
            sent();
            return ResponseTemplate::new(200).set_delay(Duration::from_secs(3600));
        }
        let mut s = self.state.lock();
        if path == "/api/v0/session" {
            return session();
        }
        if request.method == Method::POST {
            s.posted.push(body);
            return if self.refuse_first_start && s.posted.len() == 1 {
                ResponseTemplate::new(429)
            } else {
                json("{}")
            };
        }
        if request.method == Method::PUT {
            s.calls.push("PUT".into());
            s.cancel_asked = true;
            return ResponseTemplate::new(200);
        }
        if request.method == Method::DELETE {
            s.calls.push("DELETE".into());
            return ResponseTemplate::new(204);
        }
        if path.ends_with("/responses") {
            s.calls.push("GET responses".into());
            return json(if s.saved { ANSWERS } else { "[]" });
        }

        s.now = Some(s.now.expect("set") + TimeDelta::milliseconds(500));
        s.reads += 1;
        let reads = s.reads;
        if s.state.starts_with("Completed") {
            s.saved = true;
        } else if self.ends_after.is_some_and(|n| reads >= n) || (s.cancel_asked && !self.ignores_cancel) {
            s.state = if s.cancel_asked {
                "Completed, Cancelled".into()
            } else {
                "Completed, TimedOut".into()
            };
        }
        let ended_at = if s.saved {
            format!("\"{}\"", s.now.expect("set").to_rfc3339())
        } else {
            "null".into()
        };
        let answer = format!(
            r#"{{"state":"{}","responseCount":1,"fileCount":1,"endedAt":{ended_at}}}"#,
            s.state
        );
        drop(s);
        if let Some(read) = &self.on_status_read {
            read(reads);
        }
        json(&answer)
    }
}

async fn search_client(slskd: &FakeSearchSlskd) -> (SoulseekClient, MockServer) {
    let server = serve(slskd.clone()).await;
    let clock_source = slskd.clone();
    let client = SoulseekClient::with_timings(
        &settings(&server),
        Timings {
            search_poll_interval: Duration::from_millis(1),
            search_start_retry_delay: Duration::from_millis(1),
            clock: Clock::new(move || clock_source.now()),
            ..Timings::default()
        },
    );
    (client, server)
}

#[tokio::test]
async fn search_posts_the_profile_it_was_given() {
    let slskd = FakeSearchSlskd::new(Some(2));
    let (client, _server) = search_client(&slskd).await;
    client
        .search(
            "Artist Song",
            &SearchProfile::upgrade(&SoulseekSettings::default()),
            &CancellationToken::new(),
        )
        .await
        .expect("not cancelled");
    let posted = slskd.posted();
    assert_eq!(posted.len(), 1);
    let posted: Value = serde_json::from_str(&posted[0]).expect("JSON");
    assert_eq!(
        (posted["fileLimit"].as_i64(), posted["searchTimeout"].as_i64()),
        (Some(2_000), Some(30_000))
    );
}

#[tokio::test]
async fn a_search_that_ends_on_its_own_returns_its_responses() {
    // The fourth look already says Completed with nothing saved yet, as slskd really does;
    // the fifth has endedAt and the answers.
    let slskd = FakeSearchSlskd::new(Some(4));
    let (client, _server) = search_client(&slskd).await;
    let hits = client
        .search("Artist Song", &interactive(), &CancellationToken::new())
        .await
        .expect("not cancelled");
    client.last_search_cleanup().await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].extension, "flac");
    assert!(
        slskd.now() - start() < TimeDelta::seconds(30),
        "waited out the ceiling, so it proves nothing"
    );
    let calls = slskd.calls();
    assert!(!calls.contains(&"PUT".to_string()));
    assert_eq!(calls.last().map(String::as_str), Some("DELETE"));
}

#[tokio::test]
async fn a_search_still_running_at_the_ceiling_is_cancelled_and_its_responses_kept() {
    let slskd = FakeSearchSlskd::new(None);
    let (client, _server) = search_client(&slskd).await;
    let hits = client
        .search("Artist Song", &interactive(), &CancellationToken::new())
        .await
        .expect("not cancelled");
    client.last_search_cleanup().await;
    // Octo used to return nothing here while the answers sat in slskd.
    assert_eq!(hits.len(), 1);
    assert!(slskd.now() - start() >= TimeDelta::seconds(30));
    let calls = slskd.calls();
    let put = calls.iter().position(|c| c == "PUT").expect("a cancel");
    let read = calls.iter().position(|c| c == "GET responses").expect("a read");
    assert!(put < read);
    assert_eq!(slskd.cancels_and_deletes(), ["PUT", "DELETE"]);
}

#[tokio::test]
async fn a_search_slskd_will_not_stop_is_cancelled_again_before_it_is_deleted() {
    let mut slskd = FakeSearchSlskd::new(None);
    slskd.ignores_cancel = true;
    let (client, _server) = search_client(&slskd).await;
    let hits = client
        .search("Artist Song", &interactive(), &CancellationToken::new())
        .await
        .expect("not cancelled");
    assert!(hits.is_empty());
    client.last_search_cleanup().await;
    assert_eq!(slskd.cancels_and_deletes(), ["PUT", "PUT", "DELETE"]);
}

#[tokio::test]
async fn a_caller_who_gives_up_cancels_the_slskd_search() {
    let cts = CancellationToken::new();
    let mut slskd = FakeSearchSlskd::new(None);
    let giving_up = cts.clone();
    slskd.on_status_read = Some(Arc::new(move |n| {
        if n == 3 {
            giving_up.cancel();
        }
    }));
    let (client, _server) = search_client(&slskd).await;
    let outcome = client.search("Artist Song", &interactive(), &cts).await;
    assert_eq!(outcome, Err(SoulseekClientError::Cancelled));
    client.last_search_cleanup().await;
    assert_eq!(slskd.cancels_and_deletes(), ["PUT", "DELETE"]);
    assert!(!slskd.calls().contains(&"GET responses".to_string()));
}

#[tokio::test]
async fn a_caller_who_gives_up_while_the_start_is_on_its_way_still_cancels_the_slskd_search() {
    let cts = CancellationToken::new();
    let mut slskd = FakeSearchSlskd::new(None);
    let giving_up = cts.clone();
    slskd.on_start_sent = Some(Arc::new(move || giving_up.cancel()));
    let (client, _server) = search_client(&slskd).await;
    let outcome = client.search("Artist Song", &interactive(), &cts).await;
    assert_eq!(outcome, Err(SoulseekClientError::Cancelled));
    client.last_search_cleanup().await;
    assert_eq!(slskd.posted().len(), 1);
    assert_eq!(slskd.cancels_and_deletes(), ["PUT", "DELETE"]);
}

#[tokio::test]
async fn a_start_refused_with_too_many_requests_is_retried_once() {
    let mut slskd = FakeSearchSlskd::new(Some(2));
    slskd.refuse_first_start = true;
    let (client, _server) = search_client(&slskd).await;
    let hits = client
        .search("Artist Song", &interactive(), &CancellationToken::new())
        .await
        .expect("not cancelled");
    assert_eq!(slskd.posted().len(), 2);
    assert_eq!(hits.len(), 1);
}

/// Not in the C#: a caller that drops the search (rather than cancelling its token) leaves
/// nothing running in slskd either.
#[tokio::test]
async fn a_dropped_search_is_cleaned_up_too() {
    let slskd = FakeSearchSlskd::new(None);
    let (client, _server) = search_client(&slskd).await;
    let (profile, never) = (interactive(), CancellationToken::new());
    let search = client.search("Artist Song", &profile, &never);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), search)
            .await
            .is_err()
    );
    client.last_search_cleanup().await;
    assert_eq!(slskd.cancels_and_deletes(), ["PUT", "DELETE"]);
}

// ---- SoulseekSlowTransferTests ---------------------------------------------------------------
//
// A slow peer with the right file is waited for as long as it keeps sending. A peer
// that sends nothing for the whole window is given up on, and its transfer is
// cancelled in slskd: left running, it finished minutes later and put a second copy
// of the song in the library.

const PEER: &str = "peer";
const REMOTE_FILE: &str = r"Music\Artist\Album\13 - Song.flac";

struct TransferState {
    polls: i32,
    now: DateTime<Utc>,
    deletes: Vec<url::Url>,
}

/// slskd as far as the wait sees it: a session, the user's downloads with one transfer whose
/// state each poll decides, and cancels. slskd's clock moves half a second with every poll,
/// so the wait never depends on how busy the machine running the tests is.
#[derive(Clone)]
struct FakeTransferSlskd {
    on_poll: Arc<dyn Fn(i32) -> (&'static str, i64) + Send + Sync>,
    state: Arc<Mutex<TransferState>>,
}

impl FakeTransferSlskd {
    fn new(on_poll: impl Fn(i32) -> (&'static str, i64) + Send + Sync + 'static) -> Self {
        FakeTransferSlskd {
            on_poll: Arc::new(on_poll),
            state: Arc::new(Mutex::new(TransferState {
                polls: 0,
                now: start(),
                deletes: Vec::new(),
            })),
        }
    }

    fn deletes(&self) -> Vec<url::Url> {
        self.state.lock().deletes.clone()
    }
}

impl Respond for FakeTransferSlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if request.url.path() == "/api/v0/session" {
            return session();
        }
        let mut s = self.state.lock();
        if request.method == Method::DELETE {
            s.deletes.push(request.url.clone());
            return ResponseTemplate::new(204);
        }
        s.now += TimeDelta::milliseconds(500);
        let (state, bytes) = (self.on_poll)(s.polls);
        s.polls += 1;
        let file = REMOTE_FILE.replace('\\', "\\\\");
        json(&format!(
            r#"{{"username":"{PEER}","directories":[{{"directory":"x","files":[
              {{"id":"transfer-7","filename":"{file}","state":"{state}","size":400000,"bytesTransferred":{bytes}}}
            ]}}]}}"#
        ))
    }
}

async fn transfer_client(slskd: &FakeTransferSlskd) -> (SoulseekClient, MockServer) {
    let server = serve(slskd.clone()).await;
    let clock_source = slskd.state.clone();
    let client = SoulseekClient::with_timings(
        &settings(&server),
        Timings {
            poll_interval: Duration::from_millis(1),
            clock: Clock::new(move || clock_source.lock().now),
            ..Timings::default()
        },
    );
    (client, server)
}

#[tokio::test]
async fn a_slow_peer_that_keeps_sending_is_waited_for_past_the_window() {
    // Each poll is 0.5 s apart and brings more bytes; it finishes after 20 s, far
    // past the 1 s window.
    let slskd = FakeTransferSlskd::new(|poll| {
        if poll < 40 {
            ("InProgress", i64::from(poll) * 10_000)
        } else {
            ("Completed, Succeeded", 400_000)
        }
    });
    let (client, _server) = transfer_client(&slskd).await;

    let state = client
        .wait_for_completion(PEER, REMOTE_FILE, Some(1), &CancellationToken::new(), None, None)
        .await
        .expect("not cancelled");

    assert_eq!(state, SoulseekTransferState::Succeeded);
    assert!(
        slskd.state.lock().now - start() > TimeDelta::seconds(1),
        "finished inside the window, so it proves nothing"
    );
    assert!(slskd.deletes().is_empty());
}

#[tokio::test]
async fn a_peer_that_sends_nothing_is_cancelled_in_slskd() {
    let slskd = FakeTransferSlskd::new(|_| ("Queued, Remotely", 0));
    let (client, _server) = transfer_client(&slskd).await;

    let state = client
        .wait_for_completion(PEER, REMOTE_FILE, Some(1), &CancellationToken::new(), None, None)
        .await
        .expect("not cancelled");

    assert_eq!(state, SoulseekTransferState::Errored);
    let deletes = slskd.deletes();
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].path(), "/api/v0/transfers/downloads/peer/transfer-7");
    assert_eq!(deletes[0].query(), Some("remove=true"));
}

#[tokio::test]
async fn a_transfer_that_finished_as_it_was_given_up_is_kept_not_cancelled() {
    let slskd = FakeTransferSlskd::new(|_| ("Completed, Succeeded", 400_000));
    let (client, _server) = transfer_client(&slskd).await;

    let state = client.cancel_transfer(PEER, REMOTE_FILE, None).await;

    assert_eq!(state, SoulseekTransferState::Succeeded);
    assert!(slskd.deletes().is_empty());
}

#[tokio::test]
async fn a_transfer_slskd_no_longer_lists_has_nothing_to_cancel() {
    let slskd = FakeTransferSlskd::new(|_| ("Queued, Remotely", 0));
    let (client, _server) = transfer_client(&slskd).await;

    let state = client
        .cancel_transfer(PEER, r"Music\Someone\Else.flac", None)
        .await;

    assert_eq!(state, SoulseekTransferState::Errored);
    assert!(slskd.deletes().is_empty());
}

/// Not in the C#: the listener hears every poll that finds the transfer, and a caller who
/// gives up is told so rather than handed an outcome.
#[tokio::test]
async fn the_listener_hears_each_poll_and_a_caller_can_give_up() {
    let slskd = FakeTransferSlskd::new(|poll| {
        if poll < 3 {
            ("InProgress", i64::from(poll + 1) * 1000)
        } else {
            ("Completed, Succeeded", 400_000)
        }
    });
    let (client, _server) = transfer_client(&slskd).await;
    let heard = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let heard = heard.clone();
        move |p: &SoulseekTransferProgress| heard.lock().push(p.bytes_transferred)
    };
    let state = client
        .wait_for_completion(
            PEER,
            REMOTE_FILE,
            Some(30),
            &CancellationToken::new(),
            Some(&listener),
            Some("transfer-7"),
        )
        .await;
    assert_eq!(state, Ok(SoulseekTransferState::Succeeded));
    assert_eq!(*heard.lock(), [Some(1000), Some(2000), Some(3000), Some(400_000)]);

    let gone = CancellationToken::new();
    gone.cancel();
    assert_eq!(
        client
            .wait_for_completion(PEER, REMOTE_FILE, Some(30), &gone, None, None)
            .await,
        Err(SoulseekClientError::Cancelled)
    );
}

// ---- ParallelDownloadTests (slskd's batch API, through the gate) ---------------------------

const JOB: &str = ".octo-incoming/slskd/job";
const ACCEPTED: &str = r#"{"batch":{"transfers":[{"id":"t-1","filename":"f.flac"}]},"failures":[]}"#;

#[derive(Default)]
struct ScriptState {
    posts: i32,
    most_at_once: i32,
    busy_until: Option<std::time::Instant>,
}

/// Answers slskd's POSTs from a script, each after 30 ms, and records whether two were ever
/// in flight at once (the C# counted them in and out; here a POST that arrives before the last
/// one's answer was due overlapped it).
#[derive(Clone)]
struct ScriptedSlskd {
    answer: Arc<dyn Fn(i32) -> ResponseTemplate + Send + Sync>,
    state: Arc<Mutex<ScriptState>>,
}

impl ScriptedSlskd {
    fn new(answer: impl Fn(i32) -> ResponseTemplate + Send + Sync + 'static) -> Self {
        ScriptedSlskd {
            answer: Arc::new(answer),
            state: Arc::new(Mutex::new(ScriptState::default())),
        }
    }

    fn posts(&self) -> i32 {
        self.state.lock().posts
    }
}

impl Respond for ScriptedSlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if request.url.path() == "/api/v0/session" {
            return json(r#"{"token":"jwt","expires":4102444800}"#);
        }
        let mut s = self.state.lock();
        let now = std::time::Instant::now();
        let at_once = if s.busy_until.is_some_and(|until| now < until) {
            2
        } else {
            1
        };
        s.most_at_once = s.most_at_once.max(at_once);
        s.busy_until = Some(now + Duration::from_millis(30));
        s.posts += 1;
        (self.answer)(s.posts).set_delay(Duration::from_millis(30))
    }
}

async fn scripted_client(slskd: &ScriptedSlskd) -> (SoulseekClient, MockServer) {
    let server = serve(slskd.clone()).await;
    let client = SoulseekClient::with_timings(
        &SoulseekSettings {
            base_url: Some(server.uri()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        },
        Timings {
            search_start_retry_delay: Duration::from_millis(1),
            min_search_spacing: Duration::ZERO,
            ..Timings::default()
        },
    );
    (client, server)
}

fn status(code: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn one_file() -> Vec<(String, i64)> {
    vec![("f.flac".to_string(), 1)]
}

#[tokio::test]
async fn parallel_enqueues_never_overlap_their_posts() {
    let slskd = ScriptedSlskd::new(|_| status(201, ACCEPTED));
    let (client, _server) = scripted_client(&slskd).await;

    let files = one_file();
    let results = futures::future::join_all((0..3).map(|_| client.enqueue_batch("peer", &files, JOB))).await;

    assert!(results.iter().all(Result::is_ok));
    assert_eq!(slskd.state.lock().most_at_once, 1);
    assert_eq!(slskd.posts(), 3);
    assert_eq!(client.batches_supported(), Some(true));
}

#[tokio::test]
async fn a_429_is_sent_again() {
    let slskd = ScriptedSlskd::new(|n| {
        if n <= 2 {
            status(429, "")
        } else {
            status(201, ACCEPTED)
        }
    });
    let (client, _server) = scripted_client(&slskd).await;
    let batch = client
        .enqueue_batch("peer", &one_file(), JOB)
        .await
        .expect("queued");
    assert_eq!(batch.transfer_ids["f.flac"], "t-1");
    assert_eq!(slskd.posts(), 3);
}

#[tokio::test]
async fn a_429_that_never_clears_fails_that_peer() {
    let slskd = ScriptedSlskd::new(|_| status(429, ""));
    let (client, _server) = scripted_client(&slskd).await;
    client.set_batches_supported(Some(true));
    assert!(matches!(
        client.enqueue_batch("peer", &one_file(), JOB).await,
        Err(SoulseekClientError::Failed(_))
    ));
    assert_eq!(slskd.posts(), 1 + OPERATION_RETRIES as i32);
}

#[tokio::test]
async fn an_slskd_without_batches_is_asked_once_then_used_the_old_way() {
    for answer in [404, 405, 400] {
        let slskd = ScriptedSlskd::new(move |_| status(answer, ""));
        let (client, _server) = scripted_client(&slskd).await;
        for _ in 0..2 {
            let batch = client
                .enqueue_batch("peer", &one_file(), JOB)
                .await
                .expect("an answer");
            assert!(!batch.supported, "HTTP {answer}");
        }
        assert_eq!(slskd.posts(), 1, "HTTP {answer}");
        assert_eq!(client.batches_supported(), Some(false), "HTTP {answer}");
    }
}

#[tokio::test]
async fn once_batches_worked_a_bad_request_is_an_error() {
    let slskd = ScriptedSlskd::new(|n| {
        if n == 1 {
            status(201, ACCEPTED)
        } else {
            status(400, "bad")
        }
    });
    let (client, _server) = scripted_client(&slskd).await;
    client
        .enqueue_batch("peer", &one_file(), JOB)
        .await
        .expect("queued");
    assert_eq!(
        client.enqueue_batch("peer", &one_file(), JOB).await,
        Err(SoulseekClientError::Failed(
            "slskd batch enqueue failed: HTTP 400 bad".into()
        ))
    );
    assert_eq!(client.batches_supported(), Some(true));
}

// ---- Not in the C#: the session, the other calls ---------------------------------------------

#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
    tokens: Arc<Mutex<i32>>,
}

impl Respond for Recorder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let auth = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.seen.lock().push((
            request.method.to_string(),
            request.url.path().to_string(),
            auth.clone(),
        ));
        match request.url.path() {
            "/api/v0/session" => {
                let mut tokens = self.tokens.lock();
                *tokens += 1;
                json(&format!(r#"{{"token":"jwt-{tokens}","expires":4102444800}}"#))
            }
            // The first token is refused, as after slskd rotated its key.
            "/api/v0/application" if auth.as_deref() == Some("Bearer jwt-1") => ResponseTemplate::new(401),
            "/api/v0/application" => json(r#"{"server":{"isConnected":true,"isLoggedIn":true}}"#),
            "/api/v0/options" => {
                json(r#"{"directories":{"downloads":"/music","incomplete":"/music/incomplete"}}"#)
            }
            "/api/v0/users/a%20peer/directory" => json(r#"{"files":[{"filename":"01.flac","size":1}]}"#),
            _ => ResponseTemplate::new(404),
        }
    }
}

#[tokio::test]
async fn a_refused_token_is_renewed_once_and_the_call_sent_again() {
    let recorder = Recorder::default();
    let server = serve(recorder.clone()).await;
    let client = SoulseekClient::new(&settings(&server));

    assert!(client.is_reachable().await);
    assert_eq!(
        client.read_server().await.map(|r| r.link),
        Some(octo_core::soulseek::SoulseekLinkState::LoggedIn)
    );
    let seen = recorder.seen.lock().clone();
    let calls: Vec<(&str, Option<&str>)> = seen
        .iter()
        .map(|(_, path, auth)| (path.as_str(), auth.as_deref()))
        .collect();
    assert_eq!(
        calls,
        [
            ("/api/v0/session", None),
            ("/api/v0/application", Some("Bearer jwt-1")),
            ("/api/v0/session", None),
            ("/api/v0/application", Some("Bearer jwt-2")),
            ("/api/v0/application", Some("Bearer jwt-2")),
        ]
    );
    assert_eq!(client.base_url(), server.uri());
}

#[tokio::test]
async fn the_directories_and_a_folder_listing_are_read() {
    let recorder = Recorder::default();
    let server = serve(recorder.clone()).await;
    let client = SoulseekClient::new(&settings(&server));
    assert_eq!(client.get_downloads_directory().await.as_deref(), Some("/music"));
    assert_eq!(
        client.get_incomplete_directory().await.as_deref(),
        Some("/music/incomplete")
    );
    let from = SoulseekFileHit {
        username: "a peer".into(),
        queue_length: Some(4),
        ..Default::default()
    };
    let hits = client
        .browse_folder(&from, r"A\B", Duration::from_secs(5))
        .await
        .expect("listed");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].filename, r"A\B\01.flac");
    assert_eq!(hits[0].queue_length, Some(4));
    // A peer slskd cannot list is an empty folder.
    let stranger = SoulseekFileHit {
        username: "nobody".into(),
        ..Default::default()
    };
    assert!(
        client
            .browse_folder(&stranger, "X", Duration::from_secs(5))
            .await
            .expect("not an error")
            .is_empty()
    );
}

#[tokio::test]
async fn without_a_login_no_session_is_asked_for_and_an_unreachable_slskd_is_said_so() {
    let recorder = Recorder::default();
    let server = serve(recorder.clone()).await;
    let client = SoulseekClient::new(&SoulseekSettings {
        base_url: Some(server.uri()),
        username: Some(" ".into()),
        ..Default::default()
    });
    client.is_reachable().await;
    assert!(
        recorder
            .seen
            .lock()
            .iter()
            .all(|(_, path, auth)| path != "/api/v0/session" && auth.is_none())
    );

    let nowhere = SoulseekClient::new(&SoulseekSettings {
        base_url: Some("http://127.0.0.1:1".into()),
        ..Default::default()
    });
    assert!(!nowhere.is_reachable().await);
    assert_eq!(nowhere.read_server().await, None);
    assert_eq!(nowhere.get_downloads_directory().await, None);
    assert_eq!(
        nowhere.cancel_transfer("p", "f", None).await,
        SoulseekTransferState::Errored
    );
    assert!(matches!(
        nowhere.enqueue_download("p", "f", 1).await,
        Err(SoulseekClientError::Failed(_))
    ));
    assert_eq!(
        SoulseekClient::new(&SoulseekSettings::default()).base_url(),
        "http://localhost:5030"
    );
}
