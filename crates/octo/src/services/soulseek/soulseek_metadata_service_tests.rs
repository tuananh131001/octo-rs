//! Port of `octo.Tests/SoulseekMetadataServiceTests.cs` and the SongLengthTests that test-map
//! deferred to here (`Placeholder_*`, `CompleteSongLengths_*`, `ResolveTopDurations_*`).
//!
//! The C# built the real Deezer, YouTube and Last.fm services over a Moq'd
//! `HttpMessageHandler`; here Deezer and the yt-dlp shim answer from wiremock servers the
//! services are pointed at, and Last.fm (task 3-B's) is a fake behind the
//! [`LastFmTrackLengths`] seam that answers from the same table the C# fixture did.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use octo_core::settings::AppSettings;
use parking_lot::Mutex;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};
use octo_core::settings::SettingsStore;

/// Every url the fake catalog was asked for, and its answers by url substring (the first
/// needle the url contains, ignoring case; anything else is a 404). A delay holds every answer
/// back, so callers started together are all in flight before any hears back (the C# gate).
#[derive(Clone)]
struct Catalog {
    routes: Arc<Vec<(String, String)>>,
    calls: Arc<Mutex<Vec<String>>>,
    delay: Option<Duration>,
}

impl Respond for Catalog {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let url = request.url.to_string();
        self.calls.lock().push(url.clone());
        let lower = url.to_lowercase();
        let answer = match self
            .routes
            .iter()
            .find(|(needle, _)| lower.contains(&needle.to_lowercase()))
        {
            Some((_, body)) => ResponseTemplate::new(200).set_body_string(body.clone()),
            None => ResponseTemplate::new(404),
        };
        match self.delay {
            Some(delay) => answer.set_delay(delay),
            None => answer,
        }
    }
}

struct Harness {
    registry: Arc<ExternalIdRegistry>,
    service: SoulseekMetadataService,
    catalog: Catalog,
    _server: MockServer,
}

impl Harness {
    fn calls(&self, needle: &str) -> usize {
        self.catalog
            .calls
            .lock()
            .iter()
            .filter(|c| c.contains(needle))
            .count()
    }

    fn lookup(&self, id: &str) -> SoulseekRouting {
        self.registry.lookup(id).expect("registered").snapshot()
    }
}

fn deezer_over(base: &str) -> Arc<DeezerMetadataService> {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
    Arc::new(DeezerMetadataService::with_base_url(http, settings, base))
}

/// Builds the service with a Deezer layer answering from a url-substring map.
/// YouTube is never reached by the album paths under test.
async fn build_service(routes: Vec<(&str, &str)>) -> Harness {
    build_service_with_delay(routes, None).await
}

async fn build_service_with_delay(routes: Vec<(&str, &str)>, delay: Option<Duration>) -> Harness {
    let catalog = Catalog {
        routes: Arc::new(
            routes
                .into_iter()
                .map(|(n, b)| (n.to_string(), b.to_string()))
                .collect(),
        ),
        calls: Arc::new(Mutex::new(Vec::new())),
        delay,
    };
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(catalog.clone())
        .mount(&server)
        .await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let service = SoulseekMetadataService::new(
        Arc::new(YouTubeResolver::with_base_url(Some(&server.uri()))),
        registry.clone(),
        deezer_over(&server.uri()),
        Arc::new(CoverArtAggregator::new(Vec::new())),
        None,
    );
    Harness {
        registry,
        service,
        catalog,
        _server: server,
    }
}

const PROVIDER: &str = SoulseekMetadataService::PROVIDER_NAME;

const ALBUM_SEARCH_JSON: &str = r#"{"data":[
    {"id":1,"title":"Test Album","record_type":"album","nb_tracks":2,
     "cover_xl":"https://cdn/a.jpg","artist":{"name":"Test Artist"}}]}"#;

const ALBUM_DETAIL_JSON: &str = r#"{"id":1,"title":"Test Album",
    "cover_xl":"https://cdn/a.jpg","release_date":"1997-05-21",
    "artist":{"name":"Test Artist"},"genres":{"data":[{"name":"Rock"}]}}"#;

const ALBUM_TRACKS_JSON: &str = r#"{"total":2,"data":[
    {"title":"Track One","duration":180,"track_position":1,"disk_number":1,"artist":{"name":"Test Artist"}},
    {"title":"Track Two","duration":240,"track_position":2,"disk_number":1,"artist":{"name":"Test Artist"}}]}"#;

fn full_album_routes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("/search/album", ALBUM_SEARCH_JSON),
        ("/album/1/tracks", ALBUM_TRACKS_JSON),
        ("/album/1", ALBUM_DETAIL_JSON),
    ]
}

async fn only_album_id(h: &Harness) -> String {
    let albums = h.service.search_albums("test", 10).await;
    assert_eq!(albums.len(), 1);
    albums[0].id.clone()
}

#[tokio::test]
async fn search_albums_returns_albums_with_registry_ids() {
    let h = build_service(vec![("/search/album", ALBUM_SEARCH_JSON)]).await;

    let albums = h.service.search_albums("test", 10).await;

    assert_eq!(albums.len(), 1);
    let album = &albums[0];
    assert_eq!(album.title, "Test Album");
    assert_eq!(album.artist, "Test Artist");
    assert!(!album.is_local);
    // The external id must be the REGISTRY id, not the Deezer id: every consumer
    // round-trips through the registry.
    assert_eq!(album.external_id.as_deref(), Some(album.id.as_str()));
    let routing = h.lookup(&album.id);
    assert_eq!(routing.kind, RoutingKind::Album);
    assert_eq!(routing.external_album_id.as_deref(), Some("1"));
}

#[tokio::test]
async fn get_album_populates_songs_with_album_id_and_registry_ids() {
    let h = build_service(full_album_routes()).await;
    let album_id = only_album_id(&h).await;

    let album = h.service.get_album(PROVIDER, &album_id).await.expect("an album");

    assert_eq!(album.songs.len(), 2);
    let titles: Vec<&str> = album.songs.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Track One", "Track Two"]);
    assert_eq!(album.year, Some(1997));

    for song in &album.songs {
        // AlbumId is what makes the album clickable in native mode and what
        // DownloadMode.Album reads.
        assert_eq!(song.album_id.as_deref(), Some(album_id.as_str()));
        assert_eq!(song.album, "Test Album");
        assert_eq!(song.external_id.as_deref(), Some(song.id.as_str()));

        // The routing must carry the album too, or the download path re-derives it
        // from artist+title and can land on a compilation instead.
        let routing = h.lookup(&song.id);
        assert_eq!(routing.kind, RoutingKind::Song);
        assert_eq!(routing.album.as_deref(), Some("Test Album"));
    }

    assert_eq!(album.songs[0].duration, Some(180));
    assert_eq!(album.songs[0].track, Some(1));
}

#[tokio::test]
async fn get_song_returns_album_from_routing() {
    // Guards the tagging fix: the download path rebuilds a song from its id alone.
    let h = build_service(full_album_routes()).await;
    let album_id = only_album_id(&h).await;
    let track_id = h
        .service
        .get_album(PROVIDER, &album_id)
        .await
        .expect("an album")
        .songs[0]
        .id
        .clone();

    let song = h.service.get_song(PROVIDER, &track_id).await.expect("a song");

    assert_eq!(song.album, "Test Album");
    assert_eq!(song.title, "Track One");
}

#[tokio::test]
async fn get_album_tracklist_unresolvable_still_returns_album() {
    // An album with no resolvable tracklist must still render rather than 404.
    let h = build_service(vec![("/search/album", ALBUM_SEARCH_JSON)]).await;
    let album_id = only_album_id(&h).await;

    let album = h.service.get_album(PROVIDER, &album_id).await.expect("an album");

    assert_eq!(album.title, "Test Album");
    assert!(album.songs.is_empty());
}

fn file_track_one(h: &Harness) {
    h.registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Test Artist".into()),
        title: Some("Track One".into()),
        album: Some("Test Album".into()),
        duration: Some(200),
        ..Default::default()
    });
}

#[tokio::test]
async fn get_album_deezer_knows_the_album_but_fails_to_answer_lists_no_filed_songs() {
    // A partial list during a Deezer outage would be cached as the whole album by a
    // client that syncs, so the songs filed under it are only a stand-in when Deezer
    // has no such album at all.
    let h = build_service(vec![("/search/album", ALBUM_SEARCH_JSON)]).await;
    let album_id = only_album_id(&h).await;
    file_track_one(&h);

    let album = h.service.get_album(PROVIDER, &album_id).await.expect("an album");

    assert!(album.songs.is_empty());
}

const DEEZER_NO_DATA: &str = r#"{"error":{"type":"DataException","message":"no data","code":800}}"#;

const DEEZER_QUOTA: &str = r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#;

/// An album row opened with one song filed under it, the catalog answering the
/// album and its tracklist with these.
async fn open_album_with_a_filed_song(album_json: &str, tracks_json: &str) -> Album {
    let h = build_service(vec![
        ("/search/album", ALBUM_SEARCH_JSON),
        ("/album/1/tracks", tracks_json),
        ("/album/1", album_json),
    ])
    .await;
    let album_id = only_album_id(&h).await;
    file_track_one(&h);
    h.service.get_album(PROVIDER, &album_id).await.expect("an album")
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|s| s.title.as_str()).collect()
}

#[tokio::test]
async fn get_album_deezer_has_no_such_album_lists_the_filed_songs() {
    // Deezer answering that the album does not exist used to read the same as an outage,
    // so the album opened empty though Octo had shown a song under it (#59).
    let album = open_album_with_a_filed_song(DEEZER_NO_DATA, ALBUM_TRACKS_JSON).await;

    assert_eq!(titles(&album.songs), ["Track One"]);
    assert_eq!(album.song_count, Some(1));
}

#[tokio::test]
async fn get_album_deezer_lists_no_tracks_for_the_album_lists_the_filed_songs() {
    let album = open_album_with_a_filed_song(ALBUM_DETAIL_JSON, r#"{"data":[]}"#).await;

    assert_eq!(titles(&album.songs), ["Track One"]);
}

#[tokio::test]
async fn get_album_deezer_says_the_album_has_no_tracks_lists_the_filed_songs() {
    let album = open_album_with_a_filed_song(
        r#"{"id":1,"title":"Test Album","nb_tracks":0,"artist":{"name":"Test Artist"}}"#,
        r#"{"total":0,"data":[]}"#,
    )
    .await;

    assert_eq!(titles(&album.songs), ["Track One"]);
    assert_eq!(album.title, "Test Album");
}

#[tokio::test]
async fn get_album_tracklist_throttled_lists_no_filed_songs() {
    // Deezer knows the album and only failed to answer: a partial list would stick.
    let album = open_album_with_a_filed_song(ALBUM_DETAIL_JSON, DEEZER_QUOTA).await;

    assert!(album.songs.is_empty());
}

#[tokio::test]
async fn get_album_unknown_id_returns_null() {
    let h = build_service(Vec::new()).await;

    assert!(h.service.get_album(PROVIDER, "nope").await.is_none());
}

#[tokio::test]
async fn get_album_wrong_provider_returns_null() {
    let h = build_service(vec![("/search/album", ALBUM_SEARCH_JSON)]).await;
    let album_id = only_album_id(&h).await;

    assert!(h.service.get_album("deezer", &album_id).await.is_none());
}

/// The album a song row names for a song Deezer cannot place: the row's own title, minted
/// as the response builder mints it for getSong and search3 (`ConvertSongToJson`, task 3-A's:
/// an album routing of the row's artist and its album, or its title when it has none).
fn album_id_from_song_row(h: &Harness, artist: &str, title: &str) -> String {
    h.registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some(artist.into()),
        album: Some(title.into()),
        ..Default::default()
    })
}

#[tokio::test]
async fn get_album_song_row_album_deezer_cannot_name_lists_the_song_that_named_it() {
    // Issue #59: an upload Deezer does not know gets a single-style album named after
    // itself. getAlbum answered it with no songs, and Tempo crashed opening the player.
    let h = build_service(Vec::new()).await;
    let song_id = h.registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: Some("yt-raya".into()),
        artist: Some("Phonk".into()),
        title: Some("Zericxxn - Raya".into()),
        duration: Some(151),
        ..Default::default()
    });
    let album_id = album_id_from_song_row(&h, "Phonk", "Zericxxn - Raya");

    let album = h.service.get_album(PROVIDER, &album_id).await.expect("an album");

    assert_eq!(album.songs.len(), 1);
    let song = &album.songs[0];
    assert_eq!(song.id, song_id);
    assert_eq!(song.album_id.as_deref(), Some(album_id.as_str()));
    assert_eq!(song.album, album.title);
    assert_eq!(album.song_count, Some(1));
}

#[tokio::test]
async fn get_album_song_row_album_lists_each_recording_once_under_its_newest_id() {
    // The same upload minted twice (a lookup found its video, so its id changed) is one song.
    let h = build_service(Vec::new()).await;
    let song = |yt: Option<&str>, artist: &str, duration: i32| SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: yt.map(str::to_string),
        artist: Some(artist.into()),
        title: Some("Zericxxn - Raya".into()),
        duration: Some(duration),
        ..Default::default()
    };
    let older = h.registry.register(song(None, "Phonk", 180));
    let newer = h.registry.register(song(Some("yt-raya"), "Phonk", 151));
    h.registry.register(song(None, "Someone Else", 151));
    let album_id = album_id_from_song_row(&h, "Phonk", "Zericxxn - Raya");

    let album = h.service.get_album(PROVIDER, &album_id).await.expect("an album");

    assert_eq!(album.songs.len(), 1);
    assert_eq!(album.songs[0].id, newer);
    assert_ne!(album.songs[0].id, older);
}

// ---- Which catalog artist an artist page lists ----------------------------------
// The page used to trust the first search hit, and two artists can share a name.

fn outside_artist(h: &Harness, name: &str, deezer_id: Option<&str>) -> String {
    h.registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some(name.into()),
        external_artist_id: deezer_id.map(str::to_string),
        ..Default::default()
    })
}

fn releases(titles: &[&str]) -> String {
    let items: Vec<String> = titles
        .iter()
        .enumerate()
        .map(|(i, title)| {
            format!(
                r#"{{"id":{},"title":"{title}","record_type":"album","release_date":"200{i}-01-01","nb_tracks":10}}"#,
                900 + i
            )
        })
        .collect();
    format!(r#"{{"data":[{}]}}"#, items.join(","))
}

fn album_titles(albums: &[Album]) -> Vec<&str> {
    albums.iter().map(|a| a.title.as_str()).collect()
}

#[tokio::test]
async fn get_artist_albums_skips_a_bigger_act_whose_name_contains_this_one() {
    let (wrong, right) = (releases(&["Wrong Record"]), releases(&["Right Record"]));
    let h = build_service(vec![
        (
            "/search/artist",
            r#"{"data":[
                {"id":111,"name":"Test Artist Orchestra","nb_fan":90000},
                {"id":222,"name":"Test Artist","nb_fan":10}]}"#,
        ),
        ("/artist/111/albums", &wrong),
        ("/artist/222/albums", &right),
    ])
    .await;

    let albums = h
        .service
        .get_artist_albums(PROVIDER, &outside_artist(&h, "Test Artist", None))
        .await;

    assert_eq!(album_titles(&albums), ["Right Record"]);
}

const TWO_NIRVANAS_LESS_FOLLOWED_FIRST: &str = r#"{"data":[
    {"id":111,"name":"Nirvana","nb_fan":40},
    {"id":222,"name":"Nirvana","nb_fan":9000000}]}"#;

const TWO_NIRVANAS: &str = r#"{"data":[
    {"id":222,"name":"Nirvana","nb_fan":9000000},
    {"id":111,"name":"Nirvana","nb_fan":40}]}"#;

#[tokio::test]
async fn get_artist_albums_of_two_artists_of_one_name_the_more_followed_and_remembers_it() {
    let (uk, us) = (releases(&["Local Anaesthetic"]), releases(&["Nevermind"]));
    let h = build_service(vec![
        ("/search/artist", TWO_NIRVANAS_LESS_FOLLOWED_FIRST),
        ("/artist/111/albums", &uk),
        ("/artist/222/albums", &us),
    ])
    .await;
    let id = outside_artist(&h, "Nirvana", None);

    let albums = h.service.get_artist_albums(PROVIDER, &id).await;

    assert_eq!(album_titles(&albums), ["Nevermind"]);
    // Kept on the artist, so the next visit asks for no name search.
    assert_eq!(h.lookup(&id).external_artist_id.as_deref(), Some("222"));
}

#[tokio::test]
async fn get_artist_albums_an_id_already_known_wins_over_a_name_search() {
    // The artist the user tapped is the less followed one of the name.
    let (uk, us) = (releases(&["Local Anaesthetic"]), releases(&["Nevermind"]));
    let h = build_service(vec![
        ("/search/artist", TWO_NIRVANAS),
        ("/artist/111/albums", &uk),
        ("/artist/222/albums", &us),
    ])
    .await;

    let albums = h
        .service
        .get_artist_albums(PROVIDER, &outside_artist(&h, "Nirvana", Some("111")))
        .await;

    assert_eq!(album_titles(&albums), ["Local Anaesthetic"]);
}

#[tokio::test]
async fn get_artist_albums_a_library_artist_is_the_one_sharing_its_albums() {
    // The library holds the less followed Nirvana. Its albums say so, even over an id a
    // search remembered for the name.
    let (uk, us) = (
        releases(&["Local Anaesthetic", "Dedicated to Markos III"]),
        releases(&["Nevermind", "In Utero"]),
    );
    let h = build_service(vec![
        ("/search/artist", TWO_NIRVANAS),
        ("/artist/111/albums", &uk),
        ("/artist/222/albums", &us),
    ])
    .await;
    let id = outside_artist(&h, "Nirvana", Some("222"));
    let library = vec!["Local Anaesthetic".to_string()];

    let albums = h
        .service
        .get_artist_albums_for_library(PROVIDER, &id, Some(&library))
        .await;

    assert!(album_titles(&albums).contains(&"Dedicated to Markos III"));
    assert!(!album_titles(&albums).contains(&"Nevermind"));
    // The library page's choice is its own. The artist's routing is everyone's.
    assert_eq!(h.lookup(&id).external_artist_id.as_deref(), Some("222"));

    // Kept for the page's next visit all the same: no name search this time.
    let searches = h.calls("/search/artist");
    let again = h
        .service
        .get_artist_albums_for_library(PROVIDER, &id, Some(&library))
        .await;
    assert_eq!(album_titles(&albums), album_titles(&again));
    assert_eq!(h.calls("/search/artist"), searches);
}

const TWO_NIRVANAS_WITH_PICTURES: &str = r#"{"data":[
    {"id":222,"name":"Nirvana","nb_fan":9000000,"picture_xl":"https://cdn/us.jpg"},
    {"id":111,"name":"Nirvana","nb_fan":40,"picture_xl":"https://cdn/uk.jpg"}]}"#;

#[tokio::test]
async fn get_artist_albums_a_library_pages_namesake_stays_on_that_page() {
    // One listener's library holds the less followed Nirvana. Their library page picking
    // that artist used to write it onto the name's shared routing, and every listener's
    // outside page and search row for "Nirvana" then showed the library's artist.
    let (uk, us) = (releases(&["Local Anaesthetic"]), releases(&["Nevermind"]));
    let h = build_service(vec![
        ("/search/artist", TWO_NIRVANAS_WITH_PICTURES),
        ("/artist/111/albums", &uk),
        ("/artist/222/albums", &us),
    ])
    .await;
    // The library page: a name search, then the albums with the library's titles.
    let rows = h.service.search_artists("Nirvana", 5).await;
    assert_eq!(rows.len(), 1);
    let id = rows[0].id.clone();
    let library = h
        .service
        .get_artist_albums_for_library(PROVIDER, &id, Some(&["Local Anaesthetic".to_string()]))
        .await;
    assert_eq!(album_titles(&library), ["Local Anaesthetic"]);

    // Another listener's outside page for the name, and their search row.
    let outside = h.service.get_artist_albums(PROVIDER, &id).await;
    let rows = h.service.search_artists("Nirvana", 5).await;
    assert_eq!(rows.len(), 1);

    assert_eq!(album_titles(&outside), ["Nevermind"]);
    assert_eq!(rows[0].image_url.as_deref(), Some("https://cdn/us.jpg"));
    assert_eq!(h.lookup(&id).external_artist_id.as_deref(), Some("222"));
}

#[tokio::test]
async fn search_albums_a_less_followed_namesakes_album_does_not_decide_the_name() {
    // Two artists share a name. An album row and the album it opens name the catalog
    // artist who made it, and writing that onto the name's shared artist entry let the
    // first search to show the obscure one's album decide "Nirvana" for every listener,
    // across restarts.
    let (uk, us) = (releases(&["Local Anaesthetic"]), releases(&["Nevermind"]));
    let h = build_service(vec![
        (
            "/search/album",
            r#"{"data":[
                {"id":5,"title":"Local Anaesthetic","record_type":"album","nb_tracks":1,
                 "artist":{"id":111,"name":"Nirvana"}}]}"#,
        ),
        (
            "/album/5/tracks",
            r#"{"total":1,"data":[
                {"title":"Modus Vivendi","duration":200,"track_position":1,"disk_number":1,"artist":{"name":"Nirvana"}}]}"#,
        ),
        (
            "/album/5",
            r#"{"id":5,"title":"Local Anaesthetic","release_date":"1971-01-01",
                "artist":{"id":111,"name":"Nirvana"}}"#,
        ),
        ("/search/artist", TWO_NIRVANAS_WITH_PICTURES),
        ("/artist/111/albums", &uk),
        ("/artist/222/albums", &us),
    ])
    .await;

    let albums = h.service.search_albums("local anaesthetic", 10).await;
    assert_eq!(albums.len(), 1);
    let album = &albums[0];
    h.service.get_album(PROVIDER, &album.id).await;

    let rows = h.service.search_artists("Nirvana", 5).await;
    assert_eq!(rows.len(), 1);
    let artist_id = album.artist_id.clone().expect("an artist id");
    let outside = h.service.get_artist_albums(PROVIDER, &artist_id).await;

    assert_eq!(rows[0].id, artist_id);
    assert_eq!(rows[0].image_url.as_deref(), Some("https://cdn/us.jpg"));
    assert_eq!(album_titles(&outside), ["Nevermind"]);
    assert_eq!(h.lookup(&artist_id).external_artist_id.as_deref(), Some("222"));
}

#[tokio::test]
async fn search_albums_an_artist_already_settled_keeps_its_choice() {
    let h = build_service(vec![(
        "/search/album",
        r#"{"data":[
            {"id":5,"title":"Local Anaesthetic","record_type":"album","nb_tracks":6,
             "artist":{"id":111,"name":"Nirvana"}}]}"#,
    )])
    .await;
    let id = outside_artist(&h, "Nirvana", Some("222"));

    let albums = h.service.search_albums("local anaesthetic", 10).await;

    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].artist_id.as_deref(), Some(id.as_str()));
    assert_eq!(h.lookup(&id).external_artist_id.as_deref(), Some("222"));
}

#[tokio::test]
async fn artist_page_opened_with_two_requests_at_once_walks_the_catalog_once() {
    // Feishin opens an artist page with the artist and its album list at the same moment,
    // and a second client may open it too. Each request walked the catalog on its own:
    // 44 calls for one page, against a limit of 30 every 5 seconds.
    let listing: Vec<String> = (0..30)
        .map(|i| {
            format!(
                r#"{{"id":{},"title":"Record {i}","record_type":"album","release_date":"2001-01-01"}}"#,
                1000 + i
            )
        })
        .collect();
    let listing = format!(r#"{{"data":[{}]}}"#, listing.join(","));
    let h = build_service_with_delay(
        vec![
            (
                "/search/artist",
                r#"{"data":[{"id":222,"name":"Busy","nb_fan":9}]}"#,
            ),
            ("/artist/222/albums", &listing),
            ("/album/1", r#"{"nb_tracks":10}"#),
        ],
        // All three are asking before the catalog answers any of them.
        Some(Duration::from_millis(50)),
    )
    .await;
    let id = outside_artist(&h, "Busy", None);

    let first_page = async {
        h.service.get_artist(PROVIDER, &id).await;
        h.service.get_artist_albums_known_counts(PROVIDER, &id).await
    };
    let (a, b, c) = tokio::join!(
        first_page,
        h.service.get_artist_albums(PROVIDER, &id),
        h.service.get_artist_albums(PROVIDER, &id)
    );
    let lists = [a, b, c];

    // One walk: the name search, the artist's listing, and the own records of the first
    // 20 albums without a count. 22 calls, where each request used to make its own.
    assert_eq!(h.calls("/search/artist"), 1);
    assert_eq!(h.calls("/artist/222/albums"), 1);
    assert_eq!(h.calls("/album/1"), 20);
    assert_eq!(h.catalog.calls.lock().len(), 22);
    assert!(lists.iter().all(|list| list.len() == 30));
    // Both page lists show the counts that came back.
    assert_eq!(lists[1].iter().filter(|a| a.song_count == Some(10)).count(), 20);
}

#[tokio::test]
async fn search_artists_two_artists_of_one_name_are_one_row_for_the_more_followed() {
    // They would get one id, and two rows opening one page only confuse.
    let h = build_service(vec![(
        "/search/artist",
        r#"{"data":[
            {"id":111,"name":"Nirvana","nb_fan":40,"picture_xl":"https://cdn/uk.jpg"},
            {"id":222,"name":"Nirvana","nb_fan":9000000,"picture_xl":"https://cdn/us.jpg"},
            {"id":333,"name":"Nirvana Tribute","nb_fan":5}]}"#,
    )])
    .await;

    let artists = h.service.search_artists("nirvana", 10).await;

    let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["Nirvana", "Nirvana Tribute"]);
    assert_eq!(artists[0].image_url.as_deref(), Some("https://cdn/us.jpg"));
    assert_eq!(
        h.lookup(&artists[0].id).external_artist_id.as_deref(),
        Some("222")
    );
}

#[tokio::test]
async fn outside_albums_say_what_kind_of_release_they_are() {
    // The album's own record calls it an EP, and opening it says so.
    let ep_detail = ALBUM_DETAIL_JSON.replace(r#""id":1,"#, r#""id":1,"record_type":"ep","#);
    let h = build_service(vec![
        ("/search/album", ALBUM_SEARCH_JSON),
        ("/album/1/tracks", ALBUM_TRACKS_JSON),
        ("/album/1", &ep_detail),
        ("/search/artist", r#"{"data":[{"id":444,"name":"Test Artist"}]}"#),
        (
            "/artist/444/albums",
            r#"{"data":[
                {"id":5,"title":"A Single","record_type":"single","release_date":"2020-01-01","nb_tracks":1},
                {"id":6,"title":"Odd One","record_type":"mixtape","release_date":"2019-01-01","nb_tracks":9},
                {"id":7,"title":"Best Of","record_type":"compile","release_date":"2018-01-01","nb_tracks":20}]}"#,
        ),
    ])
    .await;

    let found = h.service.search_albums("test", 10).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].release_types, ["album"]);

    let opened = h
        .service
        .get_album(PROVIDER, &found[0].id)
        .await
        .expect("an album");
    assert_eq!(opened.release_types, ["ep"]);

    let page = h
        .service
        .get_artist_albums(PROVIDER, &outside_artist(&h, "Test Artist", None))
        .await;
    let by_title = |title: &str| {
        page.iter()
            .find(|a| a.title == title)
            .expect(title)
            .release_types
            .clone()
    };
    assert_eq!(by_title("A Single"), ["single"]);
    // The catalog's "compile", as MusicBrainz and so Navidrome file one: an album that is
    // a compilation. Still listed after the singles.
    assert_eq!(by_title("Best Of"), ["album", "compilation"]);
    assert_eq!(album_titles(&page), ["A Single", "Best Of", "Odd One"]);
    // A type OpenSubsonic has no name for is left unsaid rather than guessed.
    assert!(by_title("Odd One").is_empty());
}

// ---- Not in the C#: the id codec, the query split and the plain answers ---------------------

#[tokio::test]
async fn the_long_external_id_round_trips_and_a_bad_one_reads_as_nothing() {
    let routing = SoulseekRouting {
        you_tube_id: Some("abc".into()),
        artist: Some("Sigur Rós".into()),
        title: Some("Hoppípolla?".into()),
        duration: Some(268),
        ..Default::default()
    };
    let id = SoulseekMetadataService::encode_external_id(&routing);
    assert!(id.starts_with("yt|abc|"), "{id}");
    assert!(!id.contains('='), "{id}");
    let back = SoulseekMetadataService::try_decode_external_id(Some(&id)).expect("decodes");
    assert_eq!(back.artist, routing.artist);
    assert_eq!(back.title, routing.title);
    assert_eq!(back.duration, Some(268));
    assert_eq!(back.you_tube_id.as_deref(), Some("abc"));

    assert!(SoulseekMetadataService::try_decode_external_id(Some("yt|a|b")).is_none());
    assert!(SoulseekMetadataService::try_decode_external_id(Some("xx|a|b|c")).is_none());
    assert!(SoulseekMetadataService::try_decode_external_id(Some("yt|a|!!!|c")).is_none());
    assert!(SoulseekMetadataService::try_decode_external_id(Some(" ")).is_none());
    let no_length = SoulseekMetadataService::try_decode_external_id(Some("yt||QQ|VA| x")).expect("decodes");
    assert_eq!(no_length.duration, None);
    assert_eq!(no_length.artist.as_deref(), Some("A"));

    // GetSong falls back to the long form when the registry does not know the id.
    let h = build_service(Vec::new()).await;
    let song = h.service.get_song("SOULSEEK", &id).await.expect("decoded");
    assert_eq!(song.title, "Hoppípolla?");
    assert_eq!(song.duration, Some(268));
    assert!(h.service.get_song("deezer", &id).await.is_none());
}

#[tokio::test]
async fn a_free_text_search_is_split_at_its_first_space() {
    let h = build_service(Vec::new()).await;
    let songs = h.service.search_songs("  Massive Attack Teardrop ", 20).await;
    assert_eq!(songs.len(), 1);
    assert_eq!(
        (songs[0].artist.as_str(), songs[0].title.as_str()),
        ("Massive", "Attack Teardrop")
    );
    assert_eq!(songs[0].duration, Some(180));
    let one_word = h.service.search_songs("Teardrop", 20).await;
    assert_eq!(
        (one_word[0].artist.as_str(), one_word[0].title.as_str()),
        ("Teardrop", "Teardrop")
    );
    assert!(h.service.search_songs(" ", 20).await.is_empty());
    let all = h.service.search_all("Massive Attack", 20, 20, 20).await;
    assert_eq!(all.songs.len(), 1);
    assert!(all.albums.is_empty() && all.artists.is_empty());
    assert!(h.service.search_playlists("x", 20).await.is_empty());
    assert!(h.service.get_playlist(PROVIDER, "x").await.is_none());
    assert!(h.service.get_playlist_tracks(PROVIDER, "x").await.is_empty());
}

// ---- SongLengthTests: filled from each source, in order --------------------------------------

/// Deezer, Last.fm and the yt-dlp shim as far as a length lookup needs them. Anything not
/// listed is a miss, answered the way each service answers one.
#[derive(Clone, Default)]
struct LengthTables {
    deezer: Arc<Mutex<HashMap<String, i32>>>,
    last_fm: Arc<Mutex<HashMap<String, i32>>>,
    video: Arc<Mutex<HashMap<String, i32>>>,
    requests: Arc<Mutex<Vec<String>>>,
    /// Background shim requests are held back this long, so a test can keep them in flight.
    hold_background: Arc<Mutex<Option<Duration>>>,
}

fn find_ignore_case(map: &HashMap<String, i32>, key: &str) -> Option<(String, i32)> {
    map.iter()
        .find(|(k, _)| k.to_lowercase() == key.to_lowercase())
        .map(|(k, v)| (k.clone(), *v))
}

/// The Deezer half of the fixture.
#[derive(Clone)]
struct DeezerTable(LengthTables);

impl Respond for DeezerTable {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.requests.lock().push(format!(
            "deezer{}?{}",
            request.url.path(),
            request.url.query().unwrap_or("")
        ));
        let q = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let hit = find_ignore_case(&self.0.deezer.lock(), &q);
        match hit {
            Some((key, duration)) if request.url.path() == "/search" => {
                let split = key.rfind(' ').expect("an artist and a title");
                let body = serde_json::json!({"data": [{
                    "title": &key[split + 1..], "duration": duration, "artist": {"name": &key[..split]},
                }]});
                ResponseTemplate::new(200).set_body_string(body.to_string())
            }
            _ => ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#),
        }
    }
}

/// The shim half of the fixture.
#[derive(Clone)]
struct ShimTable(LengthTables);

impl Respond for ShimTable {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = request.url.query().unwrap_or("").to_string();
        self.0
            .requests
            .lock()
            .push(format!("shim{}?{query}", request.url.path()));
        let q = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let answer = match find_ignore_case(&self.0.video.lock(), &q) {
            Some((_, length)) if request.url.path() == "/meta" => ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"video_id":"vid-{q}","duration":{length}}}"#)),
            _ => ResponseTemplate::new(404),
        };
        match *self.0.hold_background.lock() {
            Some(hold) if query.contains("bg=1") => answer.set_delay(hold),
            _ => answer,
        }
    }
}

/// Last.fm's track.getInfo over the fixture's table (LastFmService with an API key).
struct LastFmTable(LengthTables);

#[async_trait]
impl LastFmTrackLengths for LastFmTable {
    fn has_api_key(&self) -> bool {
        true
    }

    async fn track_duration(&self, artist: &str, title: &str) -> Option<i32> {
        self.0.requests.lock().push(format!(
            "lastfm?method=track.getInfo&artist={artist}&track={title}"
        ));
        find_ignore_case(&self.0.last_fm.lock(), &format!("{artist}|{title}")).map(|(_, seconds)| seconds)
    }
}

struct LengthFixture {
    tables: LengthTables,
    registry: Arc<ExternalIdRegistry>,
    deezer: Arc<DeezerMetadataService>,
    service: SoulseekMetadataService,
    _servers: (MockServer, MockServer),
}

impl LengthFixture {
    async fn new() -> Self {
        let tables = LengthTables::default();
        let deezer_server = MockServer::start().await;
        Mock::given(any())
            .respond_with(DeezerTable(tables.clone()))
            .mount(&deezer_server)
            .await;
        let shim_server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ShimTable(tables.clone()))
            .mount(&shim_server)
            .await;
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let deezer = deezer_over(&deezer_server.uri());
        let service = SoulseekMetadataService::new(
            Arc::new(YouTubeResolver::with_base_url(Some(&shim_server.uri()))),
            registry.clone(),
            deezer.clone(),
            Arc::new(CoverArtAggregator::new(Vec::new())),
            Some(Arc::new(LastFmTable(tables.clone()))),
        );
        LengthFixture {
            tables,
            registry,
            deezer,
            service,
            _servers: (deezer_server, shim_server),
        }
    }

    fn deezer(&self, key: &str, seconds: i32) -> &Self {
        self.tables.deezer.lock().insert(key.into(), seconds);
        self
    }

    fn last_fm(&self, key: &str, seconds: i32) -> &Self {
        self.tables.last_fm.lock().insert(key.into(), seconds);
        self
    }

    fn video(&self, key: &str, seconds: i32) -> &Self {
        self.tables.video.lock().insert(key.into(), seconds);
        self
    }

    fn requests(&self) -> Vec<String> {
        self.tables.requests.lock().clone()
    }

    async fn search(&self, artist: &str, title: &str) -> Song {
        self.search_with(artist, title, None).await
    }

    async fn search_with(&self, artist: &str, title: &str, duration: Option<i32>) -> Song {
        let mut songs = self
            .service
            .search_songs_by_artist_title(artist, title, 1, duration)
            .await;
        assert_eq!(songs.len(), 1);
        songs.remove(0)
    }

    /// A station row with no length of its own, completed and looked up.
    async fn station_row(&self, artist: &str, title: &str) -> Song {
        let mut songs = vec![self.search(artist, title).await];
        self.service.complete_song_lengths(&mut songs);
        self.service.last_length_warm().await;
        songs.remove(0)
    }

    fn shown(&self, song: &Song) -> (Option<i32>, LengthSource) {
        self.registry.lookup(&song.id).expect("registered").shown_length()
    }
}

#[tokio::test]
async fn placeholder_carries_the_remembered_length_instead_of_the_placeholder() {
    let fixture = LengthFixture::new().await;
    let first = fixture.search("Justice", "Genesis").await;
    assert_eq!(first.duration, Some(180));

    fixture
        .registry
        .remember_length(&first.id, Some(234), LengthSource::Deezer);

    let next = fixture.search("Justice", "Genesis").await;
    assert_eq!(next.id, first.id);
    assert_eq!(next.duration, Some(234));
}

#[tokio::test]
async fn placeholder_handed_in_length_loses_to_a_remembered_deezer_length() {
    let fixture = LengthFixture::new().await;
    let first = fixture.search_with("Mr. Oizo", "Positif", Some(200)).await;
    assert_eq!(first.duration, Some(200));

    fixture
        .registry
        .remember_length(&first.id, Some(207), LengthSource::Deezer);

    let next = fixture.search_with("Mr. Oizo", "Positif", Some(200)).await;
    assert_eq!(next.duration, Some(207));
}

#[tokio::test]
async fn complete_song_lengths_deezer_is_tried_first() {
    let fixture = LengthFixture::new().await;
    fixture
        .deezer("Justice Genesis", 234)
        .last_fm("Justice|Genesis", 240);
    let song = fixture.station_row("Justice", "Genesis").await;

    assert_eq!(fixture.shown(&song), (Some(234), LengthSource::Deezer));
    assert!(!fixture.requests().iter().any(|url| url.contains("track.getInfo")));
}

#[tokio::test]
async fn complete_song_lengths_last_fm_when_deezer_has_no_match() {
    let fixture = LengthFixture::new().await;
    fixture
        .last_fm("Kavinsky|Prelude", 95)
        .video("Kavinsky Prelude", 120);
    let song = fixture.station_row("Kavinsky", "Prelude").await;

    assert_eq!(fixture.shown(&song), (Some(95), LengthSource::LastFm));
    assert!(!fixture.requests().iter().any(|url| url.contains("/meta")));
}

#[tokio::test]
async fn complete_song_lengths_video_only_when_no_metadata_length_exists() {
    let fixture = LengthFixture::new().await;
    fixture.video("Daft Punk Emotion", 417);
    let song = fixture.station_row("Daft Punk", "Emotion").await;

    assert_eq!(fixture.shown(&song), (Some(417), LengthSource::Video));
    // Length only: the video is not pinned for playback and the download expectation
    // is untouched.
    let routing = fixture.registry.lookup(&song.id).expect("registered").snapshot();
    assert_eq!(routing.you_tube_id, None);
    assert_eq!(routing.duration, None);
}

#[tokio::test]
async fn complete_song_lengths_implausible_video_leaves_the_song_without_a_length() {
    let fixture = LengthFixture::new().await;
    fixture.video("Someone Live Set", 3600);
    let song = fixture.station_row("Someone", "Live Set").await;

    assert_eq!(fixture.shown(&song), (None, LengthSource::None));
    let next = fixture.search("Someone", "Live Set").await;
    assert_eq!(next.duration, Some(180));
}

#[tokio::test]
async fn complete_song_lengths_nothing_known_invents_nothing() {
    let fixture = LengthFixture::new().await;
    let song = fixture.station_row("Nobody", "Nothing").await;

    assert_eq!(fixture.shown(&song), (None, LengthSource::None));
}

#[tokio::test]
async fn complete_song_lengths_answers_from_deezers_cache_inline() {
    // A search for the same song already asked Deezer, so this response needs no lookup.
    let fixture = LengthFixture::new().await;
    fixture.deezer("Justice Genesis", 234);
    fixture
        .deezer
        .enrich_track("Justice", "Genesis", true, false)
        .await;
    let mut songs = vec![fixture.search("Justice", "Genesis").await];

    fixture.service.complete_song_lengths(&mut songs);

    assert_eq!(songs[0].duration, Some(234));
}

#[tokio::test]
async fn complete_song_lengths_looks_up_at_most_twenty_rows_per_response() {
    let fixture = LengthFixture::new().await;
    let mut songs = Vec::new();
    for i in 0..30 {
        songs.push(fixture.search("Artist", &format!("Song {i}")).await);
    }

    fixture.service.complete_song_lengths(&mut songs);
    fixture.service.last_length_warm().await;

    // One row, one lookup: a miss may try the title alone too, but twenty rows are asked about.
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|url| url.starts_with("deezer/search?q=Artist"))
            .count(),
        20
    );
}

#[tokio::test]
async fn resolve_top_durations_implausible_video_keeps_the_shown_length() {
    // The video is still pinned for playback as before; only what the row shows is held
    // to the sane range.
    let fixture = LengthFixture::new().await;
    fixture.video("Someone Live Set", 3600);
    let mut songs = vec![fixture.search("Someone", "Live Set").await];

    fixture.service.resolve_top_durations(&mut songs, false).await;

    assert_eq!(songs[0].duration, Some(180));
    assert_eq!(
        fixture
            .registry
            .lookup(&songs[0].id)
            .expect("registered")
            .snapshot()
            .you_tube_id
            .as_deref(),
        Some("vid-Someone Live Set")
    );
}

#[tokio::test]
async fn resolve_top_durations_in_the_background_leaves_the_song_and_writes_the_routing() {
    let fixture = LengthFixture::new().await;
    fixture.video("Daft Punk Emotion", 417);
    let mut songs = vec![fixture.search("Daft Punk", "Emotion").await];

    fixture.service.resolve_top_durations(&mut songs, true).await;

    assert_eq!(songs[0].duration, Some(180));
    let routing = fixture
        .registry
        .lookup(&songs[0].id)
        .expect("registered")
        .snapshot();
    assert_eq!(routing.duration, Some(417));
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid-Daft Punk Emotion"));
    assert!(
        fixture
            .requests()
            .iter()
            .any(|url| url.contains("/meta") && url.contains("bg=1"))
    );
}

#[tokio::test]
async fn resolve_top_durations_a_background_pass_in_flight_leaves_room_for_a_foreground_lookup() {
    let fixture = LengthFixture::new().await;
    fixture.video("Daft Punk Emotion", 417);
    *fixture.tables.hold_background.lock() = Some(Duration::from_secs(3));
    let mut searched = Vec::new();
    for i in 0..8 {
        searched.push(fixture.search("Filler", &format!("Song {i}")).await);
    }
    let mut songs = vec![fixture.search("Daft Punk", "Emotion").await];

    // The pass after a search, stuck on a slow shim with every permit it can take.
    let service = fixture.service.clone();
    let background = tokio::spawn(async move {
        service.resolve_top_durations(&mut searched, true).await;
    });
    for _ in 0..200 {
        if fixture
            .requests()
            .iter()
            .filter(|url| url.contains("bg=1"))
            .count()
            >= 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // getSong's own lookup still gets a permit and shows the video's length.
    fixture.service.resolve_top_durations(&mut songs, false).await;
    assert_eq!(songs[0].duration, Some(417));

    background.await.expect("the background pass ends");
}

#[tokio::test]
async fn resolve_top_durations_in_the_background_keeps_the_video_a_play_pinned() {
    let fixture = LengthFixture::new().await;
    fixture.video("Daft Punk Emotion", 417);
    let mut songs = vec![fixture.search("Daft Punk", "Emotion").await];
    let routing = fixture.registry.lookup(&songs[0].id).expect("registered");
    {
        let mut r = routing.lock();
        r.you_tube_id = Some("playing-now".into());
        r.duration = Some(400);
    }

    fixture.service.resolve_top_durations(&mut songs, true).await;

    let r = routing.snapshot();
    assert_eq!(r.you_tube_id.as_deref(), Some("playing-now"));
    assert_eq!(r.duration, Some(400));
    assert!(!fixture.requests().iter().any(|url| url.contains("/meta")));
}

// ---- Not in the C#: the search enrichment and the prewarms ---------------------------------

#[tokio::test]
async fn a_search_page_is_enriched_from_the_catalog_and_its_misses_warmed() {
    let fixture = LengthFixture::new().await;
    fixture
        .deezer("Justice Genesis", 234)
        .video("Kavinsky Prelude", 120);
    let mut songs = vec![
        fixture.search("Justice", "Genesis").await,
        fixture.search("Kavinsky", "Prelude").await,
        Song {
            id: "local".into(),
            is_local: true,
            duration: Some(1),
            ..Default::default()
        },
    ];

    fixture.service.enrich_external_songs(&mut songs).await;
    fixture.service.last_length_warm().await;

    assert_eq!(songs[0].duration, Some(234));
    assert_eq!(songs[1].duration, Some(180));
    assert_eq!(songs[2].duration, Some(1));
    assert_eq!(fixture.shown(&songs[0]), (Some(234), LengthSource::Deezer));
    // The miss went to the background lookup, which found the video's length.
    assert_eq!(fixture.shown(&songs[1]), (Some(120), LengthSource::Video));
}

#[tokio::test]
async fn the_prewarm_pins_a_video_once_and_skips_warm_songs() {
    let fixture = LengthFixture::new().await;
    // The shim's /search answers the same table as /meta in this fixture only for /meta, so
    // a /search finds nothing and pins nothing; the request still shows the prewarm ran once.
    let songs = vec![fixture.search("Daft Punk", "Emotion").await];
    fixture.service.prewarm_you_tube_ids(&songs, 12).await;
    let searches = |f: &LengthFixture| {
        f.requests()
            .iter()
            .filter(|url| url.starts_with("shim/search"))
            .count()
    };
    assert_eq!(searches(&fixture), 1);
    fixture
        .registry
        .lookup(&songs[0].id)
        .expect("registered")
        .lock()
        .you_tube_id = Some("warm".into());
    fixture
        .service
        .prewarm_you_tube_ids_for_song_ids(&[songs[0].id.clone(), "unknown".into()], 12)
        .await;
    assert_eq!(searches(&fixture), 1);
    // Cover art for outside songs goes through the aggregator, here with no sources.
    fixture.service.prewarm_cover_art(&songs, 12).await;
}
