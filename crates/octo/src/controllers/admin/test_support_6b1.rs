//! What the admin endpoint tests share: the assembled application over a test state (the
//! `WebApplicationFactory` of the C# tests), and a small client for it.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::app::AppState;
use crate::http::pipeline::{App, app_routes, build_with};
use crate::http::static_files::StaticAssets;

/// The whole pipeline (guard, CORS, routes, catch-all) over `state`.
pub fn app(state: AppState) -> App {
    build_with(state, app_routes(&StaticAssets::default()))
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("JSON body ({e}): {}", self.text()))
    }

    pub fn has_cors(&self) -> bool {
        self.headers
            .keys()
            .any(|k| k.as_str().starts_with("access-control-"))
    }
}

/// One request through the app. A `json` body is sent with `Content-Type: application/json`.
pub async fn send(
    app: &App,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    json: Option<&str>,
) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match json {
        Some(text) => {
            req = req.header("content-type", "application/json");
            Body::from(text.to_string())
        }
        None => Body::empty(),
    };
    let res = app
        .clone()
        .oneshot(req.body(body).expect("request"))
        .await
        .expect("infallible");
    let (parts, body) = res.into_parts();
    let body = body.collect().await.expect("body").to_bytes().to_vec();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

pub async fn get(app: &App, uri: &str) -> Reply {
    send(app, Method::GET, uri, &[], None).await
}

/// A dashboard write: the `X-Octo-Admin` header the guard wants, and a JSON body.
pub async fn admin_write(app: &App, method: Method, uri: &str, json: Option<&str>) -> Reply {
    send(app, method, uri, &[("X-Octo-Admin", "1")], json).await
}
