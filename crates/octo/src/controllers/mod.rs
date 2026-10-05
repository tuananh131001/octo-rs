//! The controllers: each C# controller's actions, registered with `pub fn routes() -> RouteSet`
//! and merged into [`crate::http::pipeline::app_routes`].

pub mod admin;
