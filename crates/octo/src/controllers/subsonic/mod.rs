//! Port of `Controllers/SubSonicController.cs`: the Subsonic API Octo answers itself
//! (endpoints.md §3), split by area. Every action is registered at `rest/{name}` and
//! `rest/{name}.view` for GET and POST; any other method, and every path no action claims,
//! reaches the catch-all (`http::catch_all`, the controller's `GenericEndpoint`).
pub mod helpers_6a1;
pub mod info;
pub mod internet_radio;
pub mod native;
pub mod ping;
pub mod playlists;
pub mod search;
pub mod similar_songs;

#[cfg(test)]
mod tests_6a1;

pub mod browsing;
pub mod extensions;
pub(crate) mod helpers_6a2;
pub mod lyrics;
pub mod media;
pub mod stars;

pub(crate) mod playlist_names_6a2;

#[cfg(test)]
mod test_support_6a2;

#[cfg(test)]
mod browsing_tests_6a2;
#[cfg(test)]
mod extensions_tests_6a2;
#[cfg(test)]
mod lyrics_endpoint_tests_6a2;
#[cfg(test)]
mod media_tests_6a2;
#[cfg(test)]
mod rating_tests_6a2;
#[cfg(test)]
mod scrobble_tests_6a2;

use axum::routing::get;

use crate::http::routes::RouteSet;

/// Every route of the controller.
pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("ping", get(ping::ping).post(ping::ping))
        .subsonic(
            "getRandomSongs",
            get(ping::get_random_songs).post(ping::get_random_songs),
        )
        .subsonic(
            "getPlaylists",
            get(playlists::get_playlists).post(playlists::get_playlists),
        )
        .subsonic(
            "getPlaylist",
            get(playlists::get_playlist).post(playlists::get_playlist),
        )
        .subsonic(
            "createPlaylist",
            get(playlists::mutate_playlist).post(playlists::mutate_playlist),
        )
        .subsonic(
            "updatePlaylist",
            get(playlists::mutate_playlist).post(playlists::mutate_playlist),
        )
        .subsonic(
            "deletePlaylist",
            get(playlists::mutate_playlist).post(playlists::mutate_playlist),
        )
        .subsonic(
            "getInternetRadioStations",
            get(internet_radio::get_internet_radio_stations)
                .post(internet_radio::get_internet_radio_stations),
        )
        .subsonic(
            "createInternetRadioStation",
            get(playlists::mutate_internet_radio_station).post(playlists::mutate_internet_radio_station),
        )
        .subsonic(
            "updateInternetRadioStation",
            get(playlists::mutate_internet_radio_station).post(playlists::mutate_internet_radio_station),
        )
        .subsonic(
            "deleteInternetRadioStation",
            get(playlists::mutate_internet_radio_station).post(playlists::mutate_internet_radio_station),
        )
        // [HttpGet, HttpHead]: axum answers HEAD with the GET handler, which looks at the
        // method itself.
        .route_with_head(
            "/radio/stream/{token}",
            get(internet_radio::stream_generated_radio),
        )
        .subsonic("search3", get(search::search3).post(search::search3))
        .subsonic("search2", get(search::search3).post(search::search3))
        .subsonic(
            "getSimilarSongs",
            get(similar_songs::get_similar_songs).post(similar_songs::get_similar_songs),
        )
        .subsonic(
            "getSimilarSongs2",
            get(similar_songs::get_similar_songs).post(similar_songs::get_similar_songs),
        )
        .subsonic(
            "getAlbumInfo2",
            get(info::get_album_info2).post(info::get_album_info2),
        )
        .subsonic(
            "getAlbumInfo",
            get(info::get_album_info2).post(info::get_album_info2),
        )
        .subsonic(
            "getArtistInfo2",
            get(info::get_artist_info2).post(info::get_artist_info2),
        )
        .subsonic(
            "getArtistInfo",
            get(info::get_artist_info2).post(info::get_artist_info2),
        )
        .merge(browsing::routes())
        .merge(media::routes())
        .merge(stars::routes())
        .merge(extensions::routes())
        .merge(lyrics::routes())
}
