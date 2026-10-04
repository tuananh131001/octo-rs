//! `Controllers/SubSonicController.cs`: the Subsonic API Octo answers itself (endpoints.md §3).
//! One file per area, each with its own `routes()`; [`routes`] merges them.

pub mod browsing;
pub mod extensions;
pub(crate) mod helpers_6a2;
pub mod lyrics;
pub mod media;
pub mod stars;

#[cfg(test)]
mod test_support_6a2;

use crate::http::routes::RouteSet;

/// Every Subsonic route the controller declares.
pub fn routes() -> RouteSet {
    RouteSet::new()
        .merge(browsing::routes())
        .merge(media::routes())
        .merge(stars::routes())
        .merge(extensions::routes())
        .merge(lyrics::routes())
}
