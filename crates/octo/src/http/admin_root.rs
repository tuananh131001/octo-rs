//! `AdminController.AdminRoot` (L666): `GET /admin` (and `/admin/`, since a trailing slash is
//! ignored) redirects to the dashboard page. GET only, as `[HttpGet]` was: `HEAD /admin` and
//! other methods reach the catch-all, which answers 404 for this Octo-owned path
//! ([`RouteSet::route`] sends HEAD there).
//!
//! The AdminController port (wave 6-B) may move this handler in with the rest of the
//! controller; the route stays the same.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;

use super::routes::RouteSet;

pub fn routes() -> RouteSet {
    RouteSet::new().route("/admin", get(admin_root))
}

/// `Redirect("/admin/index.html")`: a 302 with an empty body.
pub async fn admin_root() -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::FOUND;
    res.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/admin/index.html"));
    res
}
