//! Port of `CredentialCheckTests`: the sign-in check that stands in front of outside songs:
//! what it sends Navidrome, what it keeps, for how long, and what it never keeps.
//! (`Credential_ToString_HidesTheSecret` is with `SubsonicCredential` in `octo_subsonic`.)

use super::*;
use indexmap::IndexMap;
use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A ping that accepts the token "good" for anyone, optionally slow.
struct FakePing {
    delay: Duration,
}

impl Respond for FakePing {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let good = request.url.query_pairs().any(|(k, v)| k == "t" && v == "good");
        let body = if good {
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#
        } else {
            r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#
        };
        ResponseTemplate::new(200)
            .set_body_raw(body, "application/json; charset=utf-8")
            .set_delay(self.delay)
    }
}

async fn navidrome(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(FakePing { delay })
        .mount(&server)
        .await;
    server
}

fn relay(url: &str) -> SubsonicProxyService {
    SubsonicProxyService::new(Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    })))
}

fn credential_from(pairs: &[(&str, &str)]) -> Option<SubsonicCredential> {
    let parameters: IndexMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    SubsonicCredential::from(&parameters)
}

fn credential(token: &str, salt: &str) -> SubsonicCredential {
    credential_from(&[
        ("u", "alice"),
        ("t", token),
        ("s", salt),
        ("v", "1.16.1"),
        ("c", "test"),
    ])
    .expect("a sign-in")
}

async fn calls(server: &MockServer) -> usize {
    server.received_requests().await.expect("recording").len()
}

#[tokio::test]
async fn accepted_is_kept_for_the_same_sign_in() {
    let server = navidrome(Duration::ZERO).await;
    let check = CredentialCheck::new();
    let relay = relay(&server.uri());

    for _ in 0..2 {
        let verdict = check.check(Some(&credential("good", "salt")), &relay).await;
        assert_eq!(verdict, Ok(CredentialVerdict::Accepted));
    }
    assert_eq!(calls(&server).await, 1);
}

#[tokio::test]
async fn refused_is_kept_briefly_then_asked_again() {
    let server = navidrome(Duration::ZERO).await;
    let check = CredentialCheck::with_timings(Duration::from_millis(50), Duration::from_secs(5));
    let relay = relay(&server.uri());

    for _ in 0..2 {
        let verdict = check.check(Some(&credential("bad", "salt")), &relay).await;
        assert_eq!(verdict, Ok(CredentialVerdict::Refused));
    }
    assert_eq!(calls(&server).await, 1);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let verdict = check.check(Some(&credential("bad", "salt")), &relay).await;
    assert_eq!(verdict, Ok(CredentialVerdict::Refused));
    assert_eq!(calls(&server).await, 2);
}

#[tokio::test]
async fn unreachable_is_never_kept() {
    // Nothing listens on port 1, so every ask goes out and fails.
    let check = CredentialCheck::new();
    let relay = relay("http://127.0.0.1:1");

    for _ in 0..2 {
        let verdict = check.check(Some(&credential("good", "salt")), &relay).await;
        assert_eq!(verdict, Ok(CredentialVerdict::Unreachable));
    }
    assert_eq!(check.inner.verdicts.len(), 0);
}

#[tokio::test]
async fn concurrent_checks_share_one_call() {
    let server = navidrome(Duration::from_millis(200)).await;
    let check = CredentialCheck::new();
    let relay = relay(&server.uri());
    let credential = credential("good", "salt");

    let checks = (0..5).map(|_| check.check(Some(&credential), &relay));
    let verdicts = futures::future::join_all(checks).await;

    assert!(
        verdicts.iter().all(|v| *v == Ok(CredentialVerdict::Accepted)),
        "{verdicts:?}"
    );
    assert_eq!(calls(&server).await, 1);
}

#[tokio::test]
async fn only_the_sign_in_goes_to_navidrome() {
    let server = navidrome(Duration::ZERO).await;
    let credential = credential_from(&[
        ("u", "alice"),
        ("t", "good"),
        ("s", "salt"),
        ("v", "1.16.1"),
        ("c", "Symfonium"),
        ("id", "x"),
        ("query", "y"),
        ("f", "xml"),
    ]);

    let verdict = CredentialCheck::new()
        .check(credential.as_ref(), &relay(&server.uri()))
        .await;
    assert_eq!(verdict, Ok(CredentialVerdict::Accepted));

    let requests = server.received_requests().await.expect("recording");
    assert_eq!(requests.len(), 1);
    let url = &requests[0].url;
    assert!(url.path().ends_with("/rest/ping"), "{url}");
    let query: IndexMap<String, String> = url.query_pairs().into_owned().collect();
    let mut keys: Vec<&str> = query.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["c", "f", "s", "t", "u", "v"]);
    // The client's own name, so the ping lands on the player Navidrome already keeps for it.
    assert_eq!(query["c"], "Symfonium");
    assert_eq!(query["f"], "json");
}

#[tokio::test]
async fn a_sign_in_with_no_client_name_is_sent_as_octo() {
    let server = navidrome(Duration::ZERO).await;
    let credential = credential_from(&[("u", "alice"), ("t", "good"), ("s", "salt")]);

    let _ = CredentialCheck::new()
        .check(credential.as_ref(), &relay(&server.uri()))
        .await;

    let requests = server.received_requests().await.expect("recording");
    let query: IndexMap<String, String> = requests[0].url.query_pairs().into_owned().collect();
    assert_eq!(query["c"], "octo");
    assert_eq!(query["v"], "1.16.1");
}

#[tokio::test]
async fn a_different_salt_is_a_different_sign_in() {
    let server = navidrome(Duration::ZERO).await;
    let check = CredentialCheck::new();
    let relay = relay(&server.uri());

    let _ = check.check(Some(&credential("good", "one")), &relay).await;
    let _ = check.check(Some(&credential("good", "two")), &relay).await;

    assert_eq!(calls(&server).await, 2);
}

#[tokio::test]
async fn no_sign_in_is_refused_without_asking() {
    let server = navidrome(Duration::ZERO).await;
    let credential = credential_from(&[("u", "alice")]);

    assert!(credential.is_none());
    let verdict = CredentialCheck::new()
        .check(credential.as_ref(), &relay(&server.uri()))
        .await;
    assert_eq!(verdict, Ok(CredentialVerdict::Refused));
    assert_eq!(calls(&server).await, 0);
}

#[tokio::test]
async fn a_slow_navidrome_is_unreachable_after_five_seconds() {
    assert_eq!(CredentialCheck::new().check_timeout(), Duration::from_secs(5));
    let server = navidrome(Duration::from_secs(10)).await;
    let check = CredentialCheck::with_timings(Duration::from_secs(30), Duration::from_millis(50));
    let relay = relay(&server.uri());

    let started = Instant::now();
    let verdict = check.check(Some(&credential("good", "salt")), &relay).await;
    assert_eq!(verdict, Ok(CredentialVerdict::Unreachable));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    // A timeout is never kept: the next request asks again.
    let verdict = check.check(Some(&credential("good", "salt")), &relay).await;
    assert_eq!(verdict, Ok(CredentialVerdict::Unreachable));
    assert_eq!(calls(&server).await, 2);
}

#[tokio::test]
async fn a_url_http_client_cannot_use_is_an_error_as_in_csharp() {
    let verdict = CredentialCheck::new()
        .check(Some(&credential("good", "salt")), &relay("localhost:4533"))
        .await;
    assert_eq!(
        verdict,
        Err(RelayError::NotSupported(
            "The 'localhost' scheme is not supported.".into()
        ))
    );
}

#[test]
fn status_reads_only_a_json_subsonic_answer() {
    assert_eq!(
        CredentialCheck::status(br#"{"subsonic-response":{"status":"ok"}}"#).as_deref(),
        Some("ok")
    );
    assert_eq!(
        CredentialCheck::status(br#"{"subsonic-response":{"status":1}}"#),
        None
    );
    assert_eq!(
        CredentialCheck::status(b"<subsonic-response status=\"ok\"/>"),
        None
    );
}
