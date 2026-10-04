//! Octo's settings, models and pure logic: song identity, matching, tagging plans, lyrics text.

pub mod common;
pub mod config;
pub mod fingerprint;
pub mod json;
pub mod metadata;
pub mod models;
pub mod settings;
pub mod soulseek;
pub mod tagging;
pub mod updates;

/// The release this build is, as the dashboard and the User-Agent show it (`2026.10.03.2`).
/// The release build sets `OCTO_VERSION` (the octo crate's build passes it on, since
/// `CARGO_PKG_VERSION` cannot hold the leading zero); without it, this is the last C# release.
pub const VERSION: &str = match option_env!("OCTO_VERSION") {
    Some(version) => version,
    None => "2026.10.03.2",
};
