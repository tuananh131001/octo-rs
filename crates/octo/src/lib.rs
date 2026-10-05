//! Octo: a Subsonic proxy that adds Soulseek, YouTube and Last.fm to a Navidrome library.

pub mod app;
pub mod controllers;
pub mod host;
pub mod http;
pub mod logging;
pub mod middleware;
pub mod services;
pub mod workers;

pub use host::run;
