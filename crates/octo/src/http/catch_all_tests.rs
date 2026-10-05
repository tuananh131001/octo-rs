//! The catch-all's faithful relay, through the whole pipeline, against a Navidrome stood in
//! for by wiremock (endpoints.md §2.7, §3.9 step 11).

use axum::http::{HeaderMap, Method};
use http_body_util::BodyExt;
use octo_core::settings::{AppSettings, SubsonicSettings};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::http::pipeline::{App, build_with};
use crate::http::routes::RouteSet;

fn state(url: &str) -> AppState {
    AppState::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    })
}

fn app(state: AppState) -> App {
    build_with(state, RouteSet::new())
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(app: &App, method: Method, uri: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).expect("request"))
        .await
        .expect("infallible");
    let (parts, body) = res.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

#[test]
fn octo_owned_paths_are_matched_as_the_controller_matched_them() {
    for (endpoint, owned) in [
        ("admin", true),
        ("administrator", true),
        ("Admin/nope.js", true),
        ("api/admin/whatever", true),
        ("API/ADMINISTRATOR", true),
        ("assets/x.png", true),
        ("Assets", false),
        ("favicon.ico", true),
        ("FAVICON.ICO", true),
        ("favicon.ico/x", false),
        ("rest/ping", false),
        ("api/song", false),
    ] {
        assert_eq!(is_octo_owned(endpoint), owned, "{endpoint}");
    }
}

#[test]
fn the_endpoint_is_the_decoded_path_but_an_encoded_slash() {
    assert_eq!(endpoint_of("/rest/getFoo.view"), "rest/getFoo.view");
    assert_eq!(endpoint_of("/%61dmin/x"), "admin/x");
    assert_eq!(
        endpoint_of("/api/song/a%20b%2Fc%2fd%C3%A9"),
        "api/song/a b%2Fc%2fdé"
    );
    assert_eq!(endpoint_of("/"), "");
}

#[tokio::test]
async fn an_unknown_subsonic_call_is_relayed_with_its_status_body_and_allowlisted_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/getNothingAtAll"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("Vary", "Origin")
                .insert_header("X-Not-Allowlisted", "x")
                .set_body_raw("404 page not found\n", "text/plain; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let app = app(state(&server.uri()));

    let r = send(
        &app,
        Method::GET,
        "/rest/getNothingAtAll?u=admin&id=A&id=B",
        &[],
        "",
    )
    .await;

    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body, "404 page not found\n");
    assert_eq!(r.headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(r.headers["vary"], "Origin");
    assert!(!r.headers.contains_key("x-not-allowlisted"));
    // Written as a stream, so sent chunked rather than with a length, as Kestrel did.
    assert!(!r.headers.contains_key("content-length"));
    let sent = &server.received_requests().await.expect("recording")[0];
    // The repeated id goes upstream as the client sent it, not as "A,B".
    assert_eq!(sent.url.query(), Some("u=admin&id=A&id=B"));
}

#[tokio::test]
async fn a_method_the_routes_do_not_take_is_relayed_with_that_method() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/rest/getAlbum"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"subsonic-response":{}}"#, "application/json"),
        )
        .mount(&server)
        .await;
    let app = app(state(&server.uri()));

    let r = send(&app, Method::DELETE, "/rest/getAlbum?f=json&id=x", &[], "").await;

    assert_eq!(
        (r.status, r.body.as_str()),
        (StatusCode::OK, r#"{"subsonic-response":{}}"#)
    );
}

#[tokio::test]
async fn a_json_body_is_forwarded_as_sent_with_the_parameters_in_the_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/getUser"))
        .and(query_param("username", "admin"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let app = app(state(&server.uri()));
    let body = r#"{"u": "admin", "p": "pw", "f": "json", "username": "admin"}"#;

    let r = send(
        &app,
        Method::POST,
        "/rest/getUser",
        &[("Content-Type", "application/json")],
        body,
    )
    .await;

    // Nothing upstream sets a content type, so the answer is labelled by f.
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "application/json");
    let sent = &server.received_requests().await.expect("recording")[0];
    assert_eq!(String::from_utf8_lossy(&sent.body), body);
    assert_eq!(sent.headers["content-type"], "application/json");
    assert_eq!(sent.url.query(), Some("u=admin&p=pw&f=json&username=admin"));
}

#[tokio::test]
async fn a_native_admin_login_passing_through_is_captured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "jwt", "isAdmin": true, "username": "admin", "subsonicToken": "t", "subsonicSalt": "s",
        })))
        .mount(&server)
        .await;
    let state = state(&server.uri());
    let app = app(state.clone());

    let r = send(
        &app,
        Method::POST,
        "/auth/login",
        &[("Content-Type", "application/json")],
        r#"{"username":"admin","password":"pw"}"#,
    )
    .await;

    assert_eq!(r.status, StatusCode::OK);
    assert!(state.navidrome_identity.has_admin_identity());
    assert_eq!(
        state
            .navidrome_identity
            .username_for_native_token(Some("jwt"))
            .as_deref(),
        Some("admin")
    );
}

#[tokio::test]
async fn an_unreachable_navidrome_is_the_error_envelope() {
    let app = app(state("http://127.0.0.1:1"));

    let r = send(&app, Method::GET, "/rest/getArtists?f=json", &[], "").await;

    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.body,
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Error connecting to Subsonic server: Connection refused (127.0.0.1:1)"}}}"#
    );
}

#[tokio::test]
async fn octo_owned_paths_and_the_root_are_never_relayed() {
    let server = MockServer::start().await;
    let app = app(state(&server.uri()));

    assert_eq!(
        send(&app, Method::GET, "/favicon.ico", &[], "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, Method::POST, "/%61dmin/x", &[], "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, Method::GET, "/", &[], "").await.status,
        StatusCode::BAD_REQUEST
    );
    assert!(server.received_requests().await.expect("recording").is_empty());
}
