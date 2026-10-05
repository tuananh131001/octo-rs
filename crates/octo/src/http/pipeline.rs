//! The assembled application: every route, the catch-all and the middleware, in the order
//! `Program.cs` built them (see `crate::middleware` for the order and why).
//!
//! # Registering routes (the pattern every porter follows)
//!
//! Each feature module exposes `pub fn routes() -> RouteSet` with its templates, written as the
//! C# templates were (axum syntax, the C# casing; matching ignores case and a trailing slash):
//!
//! ```ignore
//! pub fn routes() -> RouteSet {
//!     RouteSet::new()
//!         .subsonic("ping", get(ping).post(ping))           // rest/ping and rest/ping.view
//!         .route("/api/admin/settings", get(get_settings).post(save_settings))
//! }
//! ```
//!
//! and [`app_routes`] merges it. Handlers take `State<AppState>`. A method a route does not list
//! falls through to the catch-all, never a 405, and so does HEAD on a GET route unless the route
//! is registered with `route_with_head`.

use std::convert::Infallible;

use axum::extract::Request;
use axum::middleware::from_fn;
use axum::response::Response;
use tower::ServiceBuilder;
use tower::util::BoxCloneSyncService;

use super::routes::RouteSet;
use super::static_files::StaticAssets;
use super::{admin_root, catch_all};
use crate::app::AppState;
use crate::middleware::{admin_guard, cors, exception, forwarded, request_log};

/// The whole application as one service, for `axum::serve` and for tests (`oneshot`).
pub type App = BoxCloneSyncService<Request, Response, Infallible>;

/// Every route Octo answers itself. Add each ported controller's `routes()` here.
pub fn app_routes(assets: &StaticAssets) -> RouteSet {
    RouteSet::new()
        .merge(assets.routes())
        .merge(admin_root::routes())
        .merge(crate::controllers::subsonic::routes())
        .merge(crate::controllers::admin::routes())
}

pub fn build(state: AppState, assets: &StaticAssets) -> App {
    build_with(state, app_routes(assets))
}

/// The pipeline around an explicit route set (tests add their own routes this way).
pub fn build_with(state: AppState, routes: RouteSet) -> App {
    let (router, canon) = routes.finish(catch_all::catch_all);
    let router = router.with_state(state);
    let service = ServiceBuilder::new()
        .layer(from_fn(request_log::request_log))
        .layer(from_fn(forwarded::forwarded_proto))
        .layer(from_fn(admin_guard::admin_request_guard))
        .layer(from_fn(cors::cors))
        .layer(from_fn(exception::catch_panic))
        .map_request(move |mut req: Request| {
            // `Request.Path` as the client spelled it, which a few actions read (the relay
            // target of getSimilarSongs and the playlist mutations keeps the client's casing).
            // axum's router keeps an `OriginalUri` it finds rather than adding its own.
            let original = axum::extract::OriginalUri(req.uri().clone());
            req.extensions_mut().insert(original);
            canon.rewrite(&mut req);
            req
        })
        // axum adds `Allow` to whatever a route's method fallback answers; that fallback is
        // the catch-all, which Kestrel reached with no such header (no Octo answer has one).
        .map_response(|mut res: Response| {
            res.headers_mut().remove(axum::http::header::ALLOW);
            res
        })
        .service(router);
    BoxCloneSyncService::new(service)
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
