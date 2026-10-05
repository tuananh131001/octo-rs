//! The two request lines ASP.NET's hosting layer wrote at Information for every request
//! (category `Microsoft.AspNetCore.Hosting.Diagnostics`), in the same words:
//!
//! ```text
//! Request starting HTTP/1.1 GET http://host/rest/ping.view?u=a&t=*** - - -
//! Request finished HTTP/1.1 GET http://host/rest/ping.view?u=a&t=*** - 200 211 application/xml 3.1416ms
//! ```
//!
//! The shipped configuration logs at Information, so these appear by default; set
//! `Logging__LogLevel__Microsoft.AspNetCore=Warning` to hide them. Subsonic clients sign in
//! through the query string, so these lines carry credentials until the log writer redacts
//! them (`crate::logging`).
//!
//! This runs outermost, before the forwarded-headers step, as the hosting layer did, so the
//! scheme is the connection's own.

use std::time::Instant;

use axum::body::HttpBody as _;
use axum::extract::Request;
use axum::http::{HeaderMap, Version, header};
use axum::middleware::Next;
use axum::response::Response;
use tracing::info;

use crate::logging::HOSTING_DIAGNOSTICS;

pub async fn request_log(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let protocol = protocol(req.version());
    let method = req.method().to_string();
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("http://{host}{path}{query}");
    info!(
        target: HOSTING_DIAGNOSTICS,
        "Request starting {protocol} {method} {url} - {} {}",
        content_type(req.headers()),
        req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).unwrap_or("-"),
    );

    let res = next.run(req).await;

    let length = res
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        // A HEAD answer's body was dropped, so its size says nothing.
        .or_else(|| {
            (method != "HEAD")
                .then(|| res.body().size_hint().exact())
                .flatten()
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "-".to_string());
    info!(
        target: HOSTING_DIAGNOSTICS,
        "Request finished {protocol} {method} {url} - {} {length} {} {:.4}ms",
        res.status().as_u16(),
        content_type(res.headers()),
        started.elapsed().as_secs_f64() * 1000.0,
    );
    res
}

fn protocol(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/1.1",
    }
}

/// The content type as the hosting log escaped it (spaces as `+`), or `-`.
fn content_type(headers: &HeaderMap) -> String {
    match headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        Some(v) if !v.is_empty() => v.replace(' ', "+"),
        _ => "-".to_string(),
    }
}
