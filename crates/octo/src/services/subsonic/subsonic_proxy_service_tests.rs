//! Port of `SubsonicProxyServiceTests`, with Navidrome stood in for by wiremock, plus the
//! relay rules endpoints.md §2.3 and §2.7 pin.

use super::*;
use http_body_util::BodyExt;
use octo_core::settings::{AppSettings, SubsonicSettings};
use wiremock::matchers::{header as header_is, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn settings_for(url: &str) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }))
}

fn service(url: &str) -> SubsonicProxyService {
    SubsonicProxyService::new(settings_for(url))
}

/// A request-scoped service, as for a client request with these headers and no body.
fn scoped(url: &str, headers: &[(&str, &str)]) -> SubsonicProxyService {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        map.append(
            axum::http::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
            HeaderValue::from_str(v).expect("header value"),
        );
    }
    service(url).with_request(Arc::new(IncomingRequest::new(
        Method::GET,
        map,
        None,
        Bytes::new(),
    )))
}

fn params(pairs: &[(&str, &str)]) -> Parameters {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording is on")
}

async fn body_of(response: Response) -> (StatusCode, HeaderMap, Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.expect("body").to_bytes();
    (status, headers, body)
}

#[tokio::test]
async fn relay_successful_request_returns_body_and_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3, 4, 5], "application/json"))
        .mount(&server)
        .await;

    let result = service(&server.uri())
        .relay(
            "rest/ping",
            &params(&[("u", "admin"), ("p", "password"), ("v", "1.16.0")]),
        )
        .await
        .expect("relayed");

    assert_eq!(result.body.as_ref(), [1, 2, 3, 4, 5]);
    assert_eq!(result.content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn relay_builds_correct_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    service(&server.uri())
        .relay("rest/ping", &params(&[("u", "admin"), ("p", "secret")]))
        .await
        .expect("relayed");

    // The mock server saw it, so the host and port were the configured ones.
    let requests = received(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/rest/ping");
    assert_eq!(requests[0].url.query(), Some("u=admin&p=secret"));
}

/// Regression for the admin UI applying nothing until a restart. These services are
/// singletons, so capturing the settings in the constructor froze them at startup while the
/// admin UI, reading the live settings, happily showed the new value as if it had taken effect.
#[tokio::test]
async fn relay_picks_up_a_settings_change_without_being_rebuilt() {
    let first = MockServer::start().await;
    let moved = MockServer::start().await;
    for server in [&first, &moved] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }
    let settings = settings_for(&first.uri());
    let service = SubsonicProxyService::new(Arc::clone(&settings));

    // Stand in for settings.json being rewritten by the admin UI and reloaded.
    settings.set(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(moved.uri()),
            ..Default::default()
        },
        ..Default::default()
    });

    service
        .relay("rest/ping", &params(&[("u", "admin")]))
        .await
        .expect("relayed");

    // The host is the point: the relay followed the new value with no rebuild.
    assert_eq!(received(&moved).await.len(), 1);
    assert!(received(&first).await.is_empty());
}

#[tokio::test]
async fn relay_encodes_special_characters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    service(&server.uri())
        .relay(
            "rest/search3",
            &params(&[("query", "rock & roll"), ("artist", "AC/DC")]),
        )
        .await
        .expect("relayed");

    let url = received(&server).await[0].url.clone();
    // Uri.EscapeDataString: a space is %20, & is %26 and / is %2F.
    assert_eq!(url.path(), "/rest/search3");
    assert_eq!(url.query(), Some("query=rock%20%26%20roll&artist=AC%2FDC"));
}

#[tokio::test]
async fn relay_http_error_throws_exception() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let error = service(&server.uri())
        .relay("rest/ping", &params(&[("u", "admin")]))
        .await
        .expect_err("a 404 is an HttpRequestException");

    assert_eq!(
        error,
        RelayError::Http("Response status code does not indicate success: 404 (Not Found).".into())
    );
}

#[tokio::test]
async fn relay_safe_successful_request_returns_success_true() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3], "application/xml"))
        .mount(&server)
        .await;

    let result = service(&server.uri())
        .relay_safe("rest/ping", &params(&[("u", "admin")]))
        .await
        .expect("success");

    assert_eq!(result.body.as_ref(), [1, 2, 3]);
    assert_eq!(result.content_type.as_deref(), Some("application/xml"));
}

#[tokio::test]
async fn relay_safe_http_error_returns_success_false() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    assert!(
        service(&server.uri())
            .relay_safe("rest/ping", &params(&[("u", "admin")]))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn relay_safe_network_exception_returns_success_false() {
    // Nothing listens on port 1.
    assert!(
        service("http://127.0.0.1:1")
            .relay_safe("rest/ping", &params(&[("u", "admin")]))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn relay_stream_successful_request_returns_file_stream_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/stream"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3, 4, 5], "audio/mpeg"))
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[])
        .relay_stream(&params(&[("id", "song123"), ("u", "admin")]))
        .await;

    let (status, headers, body) = body_of(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(headers[header::CONTENT_LENGTH], "5");
    assert_eq!(body.as_ref(), [1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn relay_stream_http_error_returns_status_code_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    // A bare StatusCodeResult(404), which [ApiController] turned into a ProblemDetails.
    let (status, headers, body) = body_of(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/problem+json; charset=utf-8"
    );
    assert!(String::from_utf8_lossy(&body).contains("\"status\":404"));
}

#[tokio::test]
async fn relay_stream_exception_returns_object_result_with_500() {
    let response = scoped("http://127.0.0.1:1", &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    let (status, _, body) = body_of(response).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"error":"Error streaming from Subsonic: Connection refused (127.0.0.1:1)"}"#
    );
}

#[tokio::test]
async fn relay_stream_default_content_type_uses_audio_mpeg() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mpeg");
}

#[tokio::test]
async fn relay_stream_with_range_header_forwards_range_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header_is("Range", "bytes=0-1023"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 0-4/10")
                .insert_header("Accept-Ranges", "bytes")
                .set_body_raw(vec![1u8, 2, 3, 4, 5], "audio/mpeg"),
        )
        .mount(&server)
        .await;

    let response = scoped(&server.uri(), &[("Range", "bytes=0-1023")])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    let (status, headers, _) = body_of(response).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["Content-Range"], "bytes 0-4/10");
    assert_eq!(headers["Accept-Ranges"], "bytes");
    assert_eq!(received(&server).await[0].headers["range"], "bytes=0-1023");
}

#[tokio::test]
async fn relay_stream_with_if_range_header_forwards_if_range_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
        .mount(&server)
        .await;

    scoped(&server.uri(), &[("If-Range", "\"etag123\"")])
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    assert_eq!(received(&server).await[0].headers["if-range"], "\"etag123\"");
}

#[tokio::test]
async fn relay_stream_null_http_context_returns_error() {
    let response = service("http://localhost:4533")
        .relay_stream(&params(&[("id", "song123")]))
        .await;

    let (status, _, body) = body_of(response).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        String::from_utf8_lossy(&body),
        r#"{"error":"HTTP context not available"}"#
    );
}

// The rules the port relies on beyond the C# tests.

#[tokio::test]
async fn a_missing_or_relative_url_is_not_configured() {
    for url in ["", "   ", "navidrome", "http//navidrome:4533"] {
        let error = service(url)
            .relay("rest/ping", &params(&[]))
            .await
            .expect_err("not configured");
        assert_eq!(
            error,
            RelayError::NotConfigured(NOT_CONFIGURED_MESSAGE.into()),
            "{url:?}"
        );
    }
    // Absolute to .NET, but not something HttpClient can send to.
    let error = service("localhost:4533")
        .relay("rest/ping", &params(&[]))
        .await
        .expect_err("a scheme");
    assert_eq!(
        error,
        RelayError::NotSupported("The 'localhost' scheme is not supported.".into())
    );
    let error = service("/music")
        .relay("rest/ping", &params(&[]))
        .await
        .expect_err("a file path");
    assert_eq!(error, RelayError::InvalidOperation(INVALID_REQUEST_URI.into()));
}

#[test]
fn repeated_parameters_go_upstream_as_the_client_sent_them() {
    let query = parser::parse_query("id=A&id=B&u=x&f=json&songId=1&songId=2");
    let parameters = params(&[
        ("id", "A,B"),
        ("u", "x"),
        ("f", "xml"),
        ("songId", "1,2,3"),
        ("c", "Octo"),
    ]);

    let restored = restore_repeated_parameters(&parameters, Some(&query), None, false);

    let expected: Vec<(String, String)> = [
        ("id", "A"),
        ("id", "B"),
        ("u", "x"),
        // Changed by a handler: sent once, as it now reads.
        ("f", "xml"),
        ("songId", "1,2,3"),
        // Added by a handler.
        ("c", "Octo"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(restored, expected);
}

#[test]
fn empty_repeats_are_restored_too() {
    // "id=&id=B&id=" reads "B", which is unchanged, so every value goes back, empty ones included.
    let query = parser::parse_query("id=&id=B&id=");
    let restored = restore_repeated_parameters(&params(&[("id", "B")]), Some(&query), None, false);
    let ids: Vec<&str> = restored.iter().map(|(_, v)| v.as_str()).collect();
    assert_eq!(ids, ["", "B", ""]);
}

#[test]
fn unchanged_form_fields_stay_in_a_forwarded_body() {
    let form = parser::parse_query("id=A&id=B&u=x");
    let query = parser::parse_query("f=json");
    let parameters = params(&[("f", "json"), ("id", "A,B"), ("u", "y")]);

    let in_body = restore_repeated_parameters(&parameters, Some(&query), Some(&form), true);
    assert_eq!(
        in_body,
        [
            ("f".to_string(), "json".to_string()),
            ("u".to_string(), "y".to_string())
        ]
    );

    let not_in_body = restore_repeated_parameters(&parameters, Some(&query), Some(&form), false);
    let keys: Vec<&str> = not_in_body.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["f", "id", "id", "u"]);
}

#[tokio::test]
async fn the_faithful_relay_keeps_method_body_status_and_allowlisted_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("X-Nd-Authorization", "token.two")
                .insert_header("X-Not-Forwarded", "x")
                .insert_header("Vary", "Origin, Accept-Encoding")
                .insert_header("Cache-Control", "max-age=60, public")
                .insert_header("Last-Modified", "Sun, 4 Oct 2026 08:07:56 GMT")
                .set_body_raw(
                    r#"{"error":"Invalid username or password"}"#,
                    "application/json;charset=UTF-8",
                ),
        )
        .mount(&server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("X-ND-Client-Unique-Id", HeaderValue::from_static("abc"));
    headers.insert("X-Other", HeaderValue::from_static("dropped"));
    headers.append(header::ACCEPT, HeaderValue::from_static("a/b"));
    headers.append(header::ACCEPT, HeaderValue::from_static("c/d"));
    let body = Bytes::from_static(br#"{"username":"admin","password":"nope"}"#);
    let incoming = Arc::new(IncomingRequest::new(
        Method::POST,
        headers,
        Some("x=1".into()),
        body.clone(),
    ));
    let relay = service(&server.uri()).with_request(Arc::clone(&incoming));

    let raw = relay
        .relay_raw("auth/login", &incoming.parameters())
        .await
        .expect("relayed");

    assert_eq!(raw.status, 401);
    assert_eq!(raw.body.as_ref(), br#"{"error":"Invalid username or password"}"#);
    assert_eq!(
        raw.content_type.as_deref(),
        Some("application/json; charset=UTF-8")
    );
    let pairs: Vec<(&str, &str)> = raw
        .response_headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("X-Nd-Authorization", "token.two"),
            ("Last-Modified", "Sun, 04 Oct 2026 08:07:56 GMT"),
            ("Cache-Control", "public, max-age=60"),
            ("Vary", "Origin"),
            ("Vary", "Accept-Encoding"),
        ]
    );

    let sent = &received(&server).await[0];
    assert_eq!(sent.body, body.to_vec());
    // A JSON body's members are parameters, and only a url-encoded body keeps them out of the
    // query, so they go upstream twice, as the C# sent them.
    assert_eq!(sent.url.query(), Some("x=1&username=admin&password=nope"));
    assert_eq!(sent.headers["content-type"], "application/json");
    assert_eq!(sent.headers["x-nd-client-unique-id"], "abc");
    assert_eq!(sent.headers["accept"], "a/b, c/d");
    assert!(!sent.headers.contains_key("x-other"));
    assert!(!sent.headers.contains_key("accept-encoding"));
}

#[tokio::test]
async fn a_forwarded_form_body_is_not_repeated_in_the_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let body = Bytes::from_static(b"u=admin&p=pw&id=A&id=B");
    let incoming = Arc::new(IncomingRequest::new(
        Method::POST,
        headers,
        Some("f=json".into()),
        body.clone(),
    ));

    service(&server.uri())
        .with_request(Arc::clone(&incoming))
        .relay_raw("rest/getGenres.view", &incoming.parameters())
        .await
        .expect("relayed");

    let sent = &received(&server).await[0];
    assert_eq!(sent.url.path(), "/rest/getGenres.view");
    assert_eq!(sent.url.query(), Some("f=json"));
    assert_eq!(sent.body, body.to_vec());
}

#[test]
fn content_types_are_rewritten_as_media_type_header_value_wrote_them() {
    for (raw, expected) in [
        ("text/plain; charset=utf-8", Some("text/plain; charset=utf-8")),
        ("application/json", Some("application/json")),
        ("text/xml;charset=UTF-8", Some("text/xml; charset=UTF-8")),
        (
            "application/json ; charset = \"utf-8\"",
            Some("application/json; charset=\"utf-8\""),
        ),
        ("bad type", None),
        ("text/html; foo", Some("text/html; foo")),
    ] {
        assert_eq!(normalize_media_type(raw).as_deref(), expected, "{raw}");
    }
}

#[test]
fn cache_control_is_merged_and_reordered_as_dotnet_did() {
    let lines = ["max-age=60, public".to_string(), "no-cache".to_string()];
    assert_eq!(
        normalize_cache_control(&lines).as_deref(),
        Some("public, no-cache, max-age=60")
    );
    let lines = ["public, max-age=315360000".to_string()];
    assert_eq!(
        normalize_cache_control(&lines).as_deref(),
        Some("public, max-age=315360000")
    );
    let lines = ["immutable, max-age=x".to_string()];
    assert_eq!(normalize_cache_control(&lines), None);
}
