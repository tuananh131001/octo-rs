//! Port of `RequestIdentityTests`' own subject. The C# tests drove it through the scrobble and
//! search3 controllers; those wait for 6-A, so their intent is checked here against the
//! service with Navidrome's tokenInfo stood in for by wiremock.

use super::*;
use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// tokenInfo knowing `alice-key` and `bob-key`, or failing for everyone.
struct TokenInfo {
    fails: bool,
    delay: Duration,
}

impl Respond for TokenInfo {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let key = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "apiKey")
            .map(|(_, v)| v.into_owned());
        let owner = match key.as_deref() {
            Some("alice-key") if !self.fails => Some("alice"),
            Some("bob-key") if !self.fails => Some("bob"),
            _ => None,
        };
        let body = match owner {
            Some(owner) => format!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","tokenInfo":{{"username":"{owner}"}}}}}}"#
            ),
            None => r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}"#.to_string(),
        };
        ResponseTemplate::new(200)
            .set_body_raw(body, "application/json")
            .set_delay(self.delay)
    }
}

async fn navidrome(fails: bool, delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/tokenInfo"))
        .respond_with(TokenInfo { fails, delay })
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

fn params(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn calls(server: &MockServer) -> usize {
    server.received_requests().await.expect("recording").len()
}

#[test]
fn token_info_is_read_only_from_an_ok_answer() {
    assert_eq!(
        RequestIdentity::token_info_username(
            br#"{"subsonic-response":{"status":"ok","tokenInfo":{"username":" bob "}}}"#
        )
        .as_deref(),
        Some("bob")
    );
    assert_eq!(
        RequestIdentity::token_info_username(
            br#"{"subsonic-response":{"status":"failed","tokenInfo":{"username":"bob"}}}"#
        ),
        None
    );
    assert_eq!(
        RequestIdentity::token_info_username(
            br#"{"subsonic-response":{"status":"ok","tokenInfo":{"username":""}}}"#
        ),
        None
    );
    assert_eq!(
        RequestIdentity::token_info_username(b"<subsonic-response status=\"ok\"/>"),
        None
    );
}

#[tokio::test]
async fn a_named_request_is_never_looked_up() {
    let server = navidrome(false, Duration::ZERO).await;
    let identity = RequestIdentity::new();
    let relay = relay(&server.uri());

    let named = identity
        .username(&params(&[("u", " carol "), ("apiKey", "bob-key")]), &relay)
        .await;
    assert_eq!(named, Ok(Some("carol".to_string())));
    let nobody = identity.username(&params(&[("u", "  ")]), &relay).await;
    assert_eq!(nobody, Ok(None));
    assert_eq!(calls(&server).await, 0);
}

/// `ApiKeyScrobble_ReachesTheKeyOwnersLastFm` and `TwoApiKeyUsers_NeverShareASearchOrder`:
/// each key is named after its owner, asked once, then remembered.
#[tokio::test]
async fn an_api_key_is_named_once_then_remembered() {
    let server = navidrome(false, Duration::ZERO).await;
    let identity = RequestIdentity::new();
    let relay = relay(&server.uri());
    let bob = params(&[
        ("apiKey", "bob-key"),
        ("v", "1.16.1"),
        ("c", "x"),
        ("f", "xml"),
        ("id", "7"),
    ]);

    assert_eq!(identity.username(&bob, &relay).await, Ok(Some("bob".into())));
    assert_eq!(identity.username(&bob, &relay).await, Ok(Some("bob".into())));
    assert_eq!(
        identity
            .username(&params(&[("apiKey", "alice-key")]), &relay)
            .await,
        Ok(Some("alice".into()))
    );
    assert_eq!(calls(&server).await, 2);

    // Only the key, the version and the client go to Navidrome, always as JSON.
    let requests = server.received_requests().await.expect("recording");
    let query: Vec<(String, String)> = requests[0].url.query_pairs().into_owned().collect();
    let keys: Vec<&str> = query.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["apiKey", "f", "v", "c"]);
    assert!(query.contains(&("f".into(), "json".into())));
}

/// `ApiKeyScrobble_WhenNavidromeWillNotSayWhose_LearnsNothing` and
/// `ApiKey_NavidromeWouldNotName_IsNotAskedAgainStraightAway`.
#[tokio::test]
async fn a_key_navidrome_will_not_name_is_nobody_and_not_asked_again_straight_away() {
    let server = navidrome(true, Duration::ZERO).await;
    let identity = RequestIdentity::new();
    let relay = relay(&server.uri());

    for _ in 0..3 {
        assert_eq!(
            identity.username(&params(&[("apiKey", "bob-key")]), &relay).await,
            Ok(None)
        );
    }
    assert_eq!(calls(&server).await, 1);
}

/// `ApiKey_NavidromeWouldNotName_IsAskedAgainAfterAWhile`.
#[tokio::test]
async fn a_key_navidrome_would_not_name_is_asked_again_after_a_while() {
    let failing = navidrome(true, Duration::ZERO).await;
    let identity = RequestIdentity::with_unnamed_lifetime(Duration::from_millis(1));

    assert_eq!(
        identity
            .username(&params(&[("apiKey", "bob-key")]), &relay(&failing.uri()))
            .await,
        Ok(None)
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let answering = navidrome(false, Duration::ZERO).await;
    assert_eq!(
        identity
            .username(&params(&[("apiKey", "bob-key")]), &relay(&answering.uri()))
            .await,
        Ok(Some("bob".into()))
    );
    assert_eq!(calls(&failing).await + calls(&answering).await, 2);
}

/// `ApiKey_RequestsArrivingTogether_AskOnce`.
#[tokio::test]
async fn requests_arriving_together_ask_once() {
    let server = navidrome(false, Duration::from_millis(300)).await;
    let identity = RequestIdentity::new();
    let relay = relay(&server.uri());
    let bob = params(&[("apiKey", "bob-key")]);

    let answers = futures::future::join_all((0..4).map(|_| identity.username(&bob, &relay))).await;

    assert!(
        answers.iter().all(|a| *a == Ok(Some("bob".to_string()))),
        "{answers:?}"
    );
    assert_eq!(calls(&server).await, 1);
}

#[tokio::test]
async fn an_unreachable_navidrome_names_nobody_and_is_remembered_briefly() {
    let identity = RequestIdentity::new();
    let relay = relay("http://127.0.0.1:1");

    assert_eq!(
        identity.username(&params(&[("apiKey", "bob-key")]), &relay).await,
        Ok(None)
    );
    assert_eq!(identity.remembered(), 1);
}
