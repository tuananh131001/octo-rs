//! The controllers: `octo/Controllers/*.cs`, one module per controller. Each registers its
//! actions with `pub fn routes() -> RouteSet`, merged into [`crate::http::pipeline::app_routes`].

pub mod admin;
pub mod subsonic;
