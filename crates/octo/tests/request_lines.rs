//! `LogRedactionTests.RequestLines_*`: the hosting layer's request lines never carry a secret.
//!
//! These live in their own test binary with one global subscriber. A scoped (thread-local)
//! subscriber in the unit tests races with other tests that hit the same request-line callsites
//! without one: tracing can cache those callsites as disabled between computing and publishing
//! their interest.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use octo::app::AppState;
use octo::http::pipeline::{App, build_with};
use octo::http::routes::RouteSet;
use octo::logging::{HOSTING_DIAGNOSTICS, RedactingMakeWriter};
use octo_core::settings::AppSettings;
use tower::ServiceExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

const SECRET_NAMES: [&str; 16] = [
    "t", "s", "p", "apiKey", "token", "api_key", "client", "sk", "api_sig", "T", "APIKEY", "Token",
    "API_KEY", "Client", "user", "User",
];

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.clone())
    }
}

/// The process-wide log, installed once for every test in this binary.
fn log() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(EnvFilter::new("info")).with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(RedactingMakeWriter::new(capture.clone())),
        );
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        capture
    })
}

fn app() -> App {
    let routes = RouteSet::new().subsonic("ping", get(|| async { "pong" }));
    build_with(AppState::for_tests(AppSettings::default()), routes)
}

fn secret() -> String {
    format!("Secret{}", uuid::Uuid::new_v4().simple())
}

/// Sends one GET and returns its status and the request lines naming `marker`.
async fn request(uri: &str, marker: &str) -> (StatusCode, Vec<String>, String) {
    let log = log();
    let req = Request::get(uri)
        .header("Host", "octo.test")
        .body(Body::empty())
        .expect("request");
    let res = app().oneshot(req).await.expect("infallible");
    let status = res.status();
    res.into_body().collect().await.expect("body");
    let text = String::from_utf8_lossy(&log.0.lock().expect("capture lock")).into_owned();
    let lines = text
        .lines()
        .filter(|l| l.contains(HOSTING_DIAGNOSTICS) && l.contains(marker))
        .map(str::to_string)
        .collect();
    (status, lines, text)
}

#[tokio::test]
async fn request_lines_mask_each_secret_parameter() {
    for name in SECRET_NAMES {
        let secret = secret();
        let marker = format!("Octo{}", uuid::Uuid::new_v4().simple());
        let (status, lines, all) = request(
            &format!("/rest/ping.view?u=winters&{name}={secret}&v=1.16.1&c={marker}&f=json"),
            &marker,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let starting: Vec<&String> = lines.iter().filter(|l| l.contains("Request starting")).collect();
        let finished: Vec<&String> = lines.iter().filter(|l| l.contains("Request finished")).collect();
        assert_eq!((starting.len(), finished.len()), (1, 1), "{name}: {lines:?}");
        for line in [starting[0], finished[0]] {
            assert!(
                line.contains(&format!(
                    "/rest/ping.view?u=winters&{name}=***&v=1.16.1&c={marker}&f=json"
                )),
                "{name}: {line}"
            );
        }
        assert!(!all.contains(&secret), "{name}");
    }
}

#[tokio::test]
async fn request_lines_mask_every_secret_at_once_and_keep_the_rest() {
    let (t, s, p, api_key, token) = (
        secret(),
        secret(),
        format!("enc:{}", secret()),
        secret(),
        secret(),
    );
    let marker = format!("Octo{}", uuid::Uuid::new_v4().simple());
    let (_, lines, all) = request(
        &format!(
            "/rest/star.view?u=winters&t={t}&s={s}&p={p}&apiKey={api_key}&token={token}&v=1.16.1&c={marker}&f=json&id=42"
        ),
        &marker,
    )
    .await;
    let finished = lines
        .iter()
        .find(|l| l.contains("Request finished"))
        .expect("finished line");
    assert!(
        finished.contains(&format!(
            "http://octo.test/rest/star.view?u=winters&t=***&s=***&p=***&apiKey=***&token=***&v=1.16.1&c={marker}&f=json&id=42"
        )),
        "{finished}"
    );
    // Not routed yet, so the catch-all relays it; with no Navidrome URL, its 200 error envelope.
    assert!(finished.contains(" - 200 "), "{finished}");
    for secret in [t, s, p, api_key, token] {
        assert!(!all.contains(&secret));
    }
}

/// `AnApiKeySignIn_IsNamed_WithoutTheKeyReachingTheLog`: an API key sign-in carries no
/// username, so Octo asks Navidrome whose key it is. That call carries the key too, and neither
/// it nor Octo's own lines about it may log it. The C# drove it through scrobble and search3
/// (6-A); here a relayed call carries the key through the catch-all, and `RequestIdentity`
/// names it.
#[tokio::test]
async fn an_api_key_sign_in_is_named_without_the_key_reaching_the_log() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let log = log();
    let navidrome = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/tokenInfo"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"bob"}}}"#,
            "application/json",
        ))
        .mount(&navidrome)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"subsonic-response":{}}"#, "application/json"),
        )
        .mount(&navidrome)
        .await;
    let mut settings = AppSettings::default();
    settings.subsonic.url = Some(navidrome.uri());
    let state = AppState::for_tests(settings);
    let app = build_with(state.clone(), RouteSet::new());
    let key = secret();

    let req = Request::get(format!("/rest/getUser.view?apiKey={key}&v=1.16.1&c=Octo&f=json"))
        .header("Host", "octo.test")
        .body(Body::empty())
        .expect("request");
    let res = app.oneshot(req).await.expect("infallible");
    assert_eq!(res.status(), StatusCode::OK);
    res.into_body().collect().await.expect("body");

    let parameters: indexmap::IndexMap<String, String> =
        [("apiKey", key.as_str()), ("v", "1.16.1"), ("c", "Octo")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let named = state
        .request_identity
        .username(&parameters, &state.subsonic_proxy)
        .await;
    assert_eq!(named, Ok(Some("bob".to_string())));

    let paths: Vec<String> = navidrome
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(paths.iter().any(|p| p.ends_with("/rest/tokenInfo")), "{paths:?}");
    let text = String::from_utf8_lossy(&log.0.lock().expect("capture lock")).into_owned();
    assert!(text.contains("apiKey=***"), "{text}");
    assert!(!text.contains(&key));
}
