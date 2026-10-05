//! The admin API (`/api/admin/*`): `AdminController`, `CoverUpgradeController`,
//! `LyricsAdminController` and `UpdateController`, one file per area.

pub mod cover_upgrade;
pub mod genre;
pub mod helpers_6b2;
pub mod library_actions;
pub mod lyrics_admin;
pub mod update;
pub mod upgrades;

#[cfg(test)]
#[path = "tests_6b2.rs"]
mod tests_6b2;

use crate::http::routes::RouteSet;

/// Every admin route.
pub fn routes() -> RouteSet {
    RouteSet::new()
        .merge(library_actions::routes())
        .merge(upgrades::routes())
        .merge(genre::routes())
        .merge(cover_upgrade::routes())
        .merge(lyrics_admin::routes())
        .merge(update::routes())
}
