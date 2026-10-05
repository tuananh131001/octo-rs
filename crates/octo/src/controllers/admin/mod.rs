//! `AdminController` (`[Route("api/admin")]`): the API behind the settings dashboard at
//! `/admin/`.
//!
//! Settings are persisted to settings.json, the highest-priority configuration source; once
//! written, the store's file watcher refreshes the live settings. Some settings (URLs, things
//! captured into services at startup) need a process restart to take effect; the dashboard
//! marks those, and `POST /api/admin/restart` exits so docker compose's restart policy brings
//! the container back with the new values.
//!
//! Every route here sits behind the admin request guard (`crate::middleware::admin_guard`):
//! a write needs the `X-Octo-Admin` header. The endpoints that read files also need a browse
//! sign-in ([`helpers_6b1::BrowseUser`]).

pub mod browse;
pub mod cover_upgrade;
pub mod genre;
pub mod helpers_6b1;
pub mod helpers_6b2;
pub mod lastfm;
pub mod library_actions;
pub mod lyrics_admin;
pub mod raw_config;
pub mod settings;
pub mod system;
pub mod update;
pub mod upgrades;

use axum::routing::{delete, get, post};

use crate::http::routes::RouteSet;

/// The routes of task 6-B1. `GET /admin` stays in [`crate::http::admin_root`].
pub fn routes_6b1() -> RouteSet {
    RouteSet::new()
        // Last.fm, ListenBrainz and radio.
        .route("/api/admin/lastfm/radio", get(lastfm::get_last_fm_radio))
        .route(
            "/api/admin/listenbrainz/validate",
            get(lastfm::validate_listen_brainz_get).post(lastfm::validate_listen_brainz_post),
        )
        .route("/api/admin/lastfm/scrobble", get(lastfm::get_last_fm_scrobbling))
        .route("/api/admin/lastfm/check", post(lastfm::check_last_fm_credentials))
        .route(
            "/api/admin/lastfm/scrobble/cancel",
            post(lastfm::cancel_last_fm_connect),
        )
        .route(
            "/api/admin/lastfm/scrobble/connect",
            post(lastfm::connect_last_fm),
        )
        .route("/api/admin/lastfm/scrobble/finish", post(lastfm::finish_last_fm))
        .route(
            "/api/admin/lastfm/scrobble/disconnect",
            post(lastfm::disconnect_last_fm),
        )
        .route(
            "/api/admin/lastfm/radio/refresh",
            post(lastfm::refresh_last_fm_radio),
        )
        .route(
            "/api/admin/lastfm/radio/history",
            delete(lastfm::reset_last_fm_radio),
        )
        // Discovery, the library chain, the browse sign-in and the files.
        .route("/api/admin/discover-servers", get(system::discover_servers))
        .route("/api/admin/library-status", get(system::library_status))
        .route("/api/admin/browse/auth", post(browse::browse_auth))
        .route("/api/admin/browse", get(browse::browse))
        .route("/api/admin/test-notification", post(system::test_notification))
        .route("/api/admin/browse/session", get(browse::browse_session))
        .route("/api/admin/browse/signout", post(browse::browse_sign_out))
        .route("/api/admin/downloads", get(system::downloads))
        .route("/api/admin/tags/preview", post(browse::preview_tags))
        .route("/api/admin/acquisitions", get(system::acquisitions))
        // Settings and configuration.
        .route(
            "/api/admin/settings",
            get(settings::get_settings).post(settings::save_settings),
        )
        .route(
            "/api/admin/raw-config",
            get(raw_config::get_raw_config).put(raw_config::put_raw_config),
        )
        .route("/api/admin/config-sources", get(raw_config::get_config_sources))
        // Status and process control.
        .route("/api/admin/status", get(system::get_status))
        .route("/api/admin/lidarr/options", get(system::get_lidarr_options))
        .route("/api/admin/lidarr/test", post(system::test_lidarr_connection))
        .route("/api/admin/restart", post(system::restart))
        .route(
            "/api/admin/soulseek/rejected-peers/clear",
            post(system::clear_rejected_peers),
        )
        .route(
            "/api/admin/clear-metadata-cache",
            post(system::clear_metadata_cache),
        )
}

/// Every AdminController route.
pub fn routes() -> RouteSet {
    routes_6b1().merge(routes_6b2())
}

/// The routes of task 6-B2: the library actions, questions and checks, the Better quality page,
/// genre normalisation, and the `CoverUpgradeController`, `LyricsAdminController` and
/// `UpdateController` routes.
pub fn routes_6b2() -> RouteSet {
    RouteSet::new()
        .merge(library_actions::routes())
        .merge(upgrades::routes())
        .merge(genre::routes())
        .merge(cover_upgrade::routes())
        .merge(lyrics_admin::routes())
        .merge(update::routes())
}

#[cfg(test)]
#[path = "tests_6b2.rs"]
mod tests_6b2;

#[cfg(test)]
#[path = "test_support_6b1.rs"]
mod test_support_6b1;

#[cfg(test)]
#[path = "admin_contract_tests.rs"]
mod admin_contract_tests;

#[cfg(test)]
#[path = "lastfm_admin_tests.rs"]
mod lastfm_admin_tests;
