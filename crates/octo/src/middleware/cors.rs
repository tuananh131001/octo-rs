//! `UseCors()` with Octo's default policy: `AllowAnyOrigin`, `AllowAnyMethod`,
//! `AllowAnyHeader`, and the exposed headers `X-Content-Duration`, `X-Total-Count`,
//! `X-Nd-Authorization`. Subsonic web players on any origin need it.
//!
//! Reproduces what ASP.NET's CORS middleware put on the wire (endpoints.md §7), which
//! tower-http's `CorsLayer` does not do byte for byte:
//!
//! - A request without `Origin` is left alone.
//! - A preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) is answered here
//!   with `204`, no body and no `Content-Type`: `Access-Control-Allow-Headers` echoing the
//!   requested headers (trimmed, comma-joined without spaces; absent when none were asked for),
//!   `Access-Control-Allow-Methods` echoing the requested method, and
//!   `Access-Control-Allow-Origin: *`.
//! - Any other request with `Origin` gets `Access-Control-Allow-Origin: *` and
//!   `Access-Control-Expose-Headers: X-Content-Duration,X-Total-Count,X-Nd-Authorization`,
//!   on every status, errors included. No `Vary`.
//!
//! On `/api/admin*` the admin guard, which runs outside this layer, answers the preflight first
//! and strips these headers again.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

/// The exposed headers, as ASP.NET joined them.
pub const EXPOSED_HEADERS: &str = "X-Content-Duration,X-Total-Count,X-Nd-Authorization";

pub async fn cors(req: Request, next: Next) -> Response {
    if !req.headers().contains_key(header::ORIGIN) {
        return next.run(req).await;
    }
    if req.method() == Method::OPTIONS
        && let Some(requested_method) = req.headers().get(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        return preflight(requested_method.clone(), &req);
    }
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSED_HEADERS),
    );
    res
}

fn preflight(requested_method: HeaderValue, req: &Request) -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::NO_CONTENT;
    let requested_headers: Vec<String> = req
        .headers()
        .get_all(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect();
    let headers = res.headers_mut();
    if !requested_headers.is_empty()
        && let Ok(v) = HeaderValue::from_str(&requested_headers.join(","))
    {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
    }
    headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, requested_method);
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    res
}
