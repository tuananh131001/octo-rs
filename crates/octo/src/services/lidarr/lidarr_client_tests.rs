//! Port of `octo.Tests/LidarrClientTests.cs`. The C# `Handler` answered every request with 200 and
//! a body chosen by method and path; here a mock server does, and keeps the requests.

use std::sync::Arc;

use octo_core::lidarr::LidarrAlbumCandidate;
use octo_core::settings::{AppSettings, LidarrSettings, SettingsStore};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::lidarr::lidarr_heart_acquisition_service::translate_imported_path;

/// Answers each request with `respond(method, path)`, always 200.
struct Handler(Box<dyn Fn(&str, &str) -> String + Send + Sync>);

impl Respond for Handler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string((self.0)(request.method.as_str(), request.url.path()))
    }
}

async fn serve(respond: impl Fn(&str, &str) -> String + Send + Sync + 'static) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(Handler(Box::new(respond)))
        .mount(&server)
        .await;
    server
}

fn build(server: &MockServer) -> LidarrClient {
    let settings = LidarrSettings {
        base_url: Some(server.uri()),
        api_key: Some("secret".into()),
        root_folder_path: Some("/data/music".into()),
        quality_profile_id: 3,
        metadata_profile_id: 4,
        ..Default::default()
    };
    LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: settings,
        ..Default::default()
    })))
}

fn candidate(foreign_id: &str, title: &str, artist: &str, year: Option<i32>) -> LidarrAlbumCandidate {
    let resource = json!({
        "foreignAlbumId": foreign_id,
        "title": title,
        "releaseDate": year.map(|y| format!("{y}-01-01T00:00:00Z")),
        "artist": { "artistName": artist, "foreignArtistId": "artist-mbid" },
    });
    LidarrAlbumCandidate {
        id: 0,
        foreign_album_id: foreign_id.into(),
        title: title.into(),
        artist: artist.into(),
        year,
        resource: resource.as_object().cloned().expect("an object"),
    }
}

fn path_and_query(request: &Request) -> String {
    match request.url.query() {
        Some(query) => format!("{}?{query}", request.url.path()),
        None => request.url.path().to_string(),
    }
}

fn api_key(request: &Request) -> Option<String> {
    request
        .headers
        .get("X-Api-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).expect("a JSON body")
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording is on")
}

#[tokio::test]
async fn options_use_api_key_and_map_server_choices() {
    let server = serve(|_, path| {
        match path {
            "/api/v1/rootfolder" => r#"[{"id":1,"path":"/data/music"}]"#,
            "/api/v1/qualityprofile" => r#"[{"id":2,"name":"Lossless"}]"#,
            "/api/v1/metadataprofile" => r#"[{"id":3,"name":"Standard"}]"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let result = build(&server).get_options().await.expect("options");

    assert_eq!(result.root_folders.len(), 1);
    assert_eq!(result.root_folders[0].path, "/data/music");
    assert_eq!(result.quality_profiles.len(), 1);
    assert_eq!(result.quality_profiles[0].name, "Lossless");
    assert_eq!(result.metadata_profiles.len(), 1);
    assert_eq!(result.metadata_profiles[0].name, "Standard");
    for request in requests(&server).await {
        assert_eq!(api_key(&request).as_deref(), Some("secret"));
    }
}

#[tokio::test]
async fn connection_test_uses_entered_url_and_api_key_without_saving() {
    let server = serve(|_, path| {
        if path == "/api/v1/system/status" {
            "{}"
        } else {
            "[]"
        }
        .to_string()
    })
    .await;
    // The saved settings point elsewhere: the entered address is the one asked.
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings::default())));

    client
        .test_connection(&format!("{}/", server.uri()), "entered-key")
        .await
        .expect("the server answers");

    let requests = requests(&server).await;
    // The mock server sees the path and the Host header; together they are the URL asked
    // (the trailing slash entered is trimmed, so no "//").
    let host = server.address().to_string();
    assert!(
        requests
            .iter()
            .any(|r| path_and_query(r) == "/api/v1/system/status"
                && r.headers.get("host").and_then(|h| h.to_str().ok()) == Some(host.as_str()))
    );
    for request in &requests {
        assert_eq!(api_key(request).as_deref(), Some("entered-key"));
    }
    for path in [
        "/api/v1/rootfolder",
        "/api/v1/qualityprofile",
        "/api/v1/metadataprofile",
    ] {
        assert!(requests.iter().any(|r| path_and_query(r) == path), "{path}");
    }
}

// AlbumSelectionRequiresExactIdentityAndUsesYearToDisambiguate is in
// octo_core::lidarr::lidarr_client's tests, with select_best_album.

#[tokio::test]
async fn new_album_uses_chosen_defaults_and_does_not_monitor_future_releases() {
    let server = serve(|method, path| {
        match (method, path) {
            ("GET", "/api/v1/album") => "[]",
            ("GET", "/api/v1/artist") => "[]",
            ("POST", "/api/v1/album") => r#"{"id":42}"#,
            ("POST", "/api/v1/command") => r#"{"id":9}"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let id = build(&server)
        .ensure_album_and_search(&candidate("mbid", "Album", "Artist", Some(2020)))
        .await
        .expect("a search");

    assert_eq!(id, 42);
    let requests = requests(&server).await;
    let adds: Vec<&Request> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/v1/album")
        .collect();
    assert_eq!(adds.len(), 1);
    let add = body(adds[0]);
    assert_eq!(add["artist"]["rootFolderPath"], "/data/music");
    assert_eq!(add["artist"]["qualityProfileId"], 3);
    assert_eq!(add["artist"]["metadataProfileId"], 4);
    assert_eq!(add["artist"]["monitorNewItems"], "none");
    assert_eq!(add["artist"]["addOptions"]["monitor"], "unknown");
    assert_eq!(add["artist"]["addOptions"]["albumsToMonitor"][0], "mbid");
    let commands: Vec<&Request> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/v1/command")
        .collect();
    assert_eq!(commands.len(), 1);
    let command = String::from_utf8_lossy(&commands[0].body);
    assert!(command.contains("AlbumSearch"));
    assert!(command.contains("42"));
}

#[tokio::test]
async fn existing_artist_settings_are_preserved_when_adding_album() {
    let server = serve(|method, path| {
        match (method, path) {
            ("GET", "/api/v1/album") => "[]",
            ("GET", "/api/v1/artist") => {
                r#"[{"id":7,"foreignArtistId":"artist-mbid","path":"/existing/Artist","qualityProfileId":9,"metadataProfileId":10}]"#
            }
            ("POST", "/api/v1/album") => r#"{"id":42}"#,
            ("POST", "/api/v1/command") => r#"{"id":9}"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    build(&server)
        .ensure_album_and_search(&candidate("mbid", "Album", "Artist", Some(2020)))
        .await
        .expect("a search");

    let requests = requests(&server).await;
    let adds: Vec<&Request> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/v1/album")
        .collect();
    assert_eq!(adds.len(), 1);
    let add = body(adds[0]);
    assert_eq!(add["artistId"], 7);
    assert_eq!(add["artist"]["path"], "/existing/Artist");
    assert_eq!(add["artist"]["qualityProfileId"], 9);
    assert_eq!(add["artist"]["metadataProfileId"], 10);
}

#[tokio::test]
async fn unmonitored_existing_artist_is_monitored_and_otherwise_unchanged() {
    let server = serve(|method, path| {
        match (method, path) {
            ("GET", "/api/v1/album") => {
                r#"[{"id":12,"foreignAlbumId":"mbid","monitored":true,"artist":{"id":7,"monitored":false}}]"#
            }
            ("GET", "/api/v1/artist/7") => {
                r#"{"id":7,"monitored":false,"monitorNewItems":"none","qualityProfileId":9}"#
            }
            ("PUT", "/api/v1/artist/7") => r#"{"id":7,"monitored":true}"#,
            ("POST", "/api/v1/command") => r#"{"id":9}"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    build(&server)
        .ensure_album_and_search(&candidate("mbid", "Album", "Artist", Some(2020)))
        .await
        .expect("a search");

    let requests = requests(&server).await;
    let puts: Vec<&Request> = requests.iter().filter(|r| r.method.as_str() == "PUT").collect();
    assert_eq!(puts.len(), 1);
    let update = body(puts[0]);
    assert_eq!(update["monitored"], true);
    assert_eq!(update["monitorNewItems"], "none");
    assert_eq!(update["qualityProfileId"], 9);
}

#[tokio::test]
async fn existing_album_is_monitored_without_changing_its_artist() {
    let server = serve(|method, path| {
        match (method, path) {
            ("GET", "/api/v1/album") => {
                r#"[{"id":12,"foreignAlbumId":"mbid","monitored":false,"artist":{"id":7,"qualityProfileId":9}}]"#
            }
            ("PUT", "/api/v1/album/12") => r#"{"id":12,"monitored":true}"#,
            ("POST", "/api/v1/command") => r#"{"id":9}"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let id = build(&server)
        .ensure_album_and_search(&candidate("mbid", "Album", "Artist", Some(2020)))
        .await
        .expect("a search");

    assert_eq!(id, 12);
    let requests = requests(&server).await;
    let puts: Vec<&Request> = requests.iter().filter(|r| r.method.as_str() == "PUT").collect();
    assert_eq!(puts.len(), 1);
    let update = body(puts[0]);
    assert_eq!(update["monitored"], true);
    assert_eq!(update["artist"]["qualityProfileId"], 9);
    assert!(!requests.iter().any(|r| r.url.path() == "/api/v1/artist"));
}

#[tokio::test]
async fn album_tracks_join_track_files_returned_by_separate_endpoint() {
    let server = serve(|_, path| {
        match path {
            "/api/v1/track" => {
                r#"[{"id":1,"title":"Song","trackNumber":"1","duration":123000,"hasFile":true,"trackFileId":8}]"#
            }
            "/api/v1/trackFile" => r#"[{"id":8,"path":"/data/music/Artist/Album/01.flac","size":456}]"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let tracks = build(&server).get_album_tracks(42).await.expect("tracks");

    assert_eq!(tracks.len(), 1);
    let track = &tracks[0];
    assert!(track.has_file);
    assert_eq!(track.path.as_deref(), Some("/data/music/Artist/Album/01.flac"));
    assert_eq!(track.size_bytes, 456);
    assert_eq!(track.duration_seconds, Some(123));
    assert_eq!(track.track_file_id, 8);
    let requests = requests(&server).await;
    assert!(
        requests
            .iter()
            .any(|r| path_and_query(r) == "/api/v1/track?albumId=42")
    );
    assert!(
        requests
            .iter()
            .any(|r| path_and_query(r) == "/api/v1/trackFile?albumId=42")
    );
}

#[tokio::test]
async fn import_completion_uses_album_statistics_not_alternate_release_rows() {
    let server = serve(|_, path| {
        match path {
            "/api/v1/album/42" => r#"{"id":42,"statistics":{"trackCount":1,"trackFileCount":1}}"#,
            "/api/v1/track" => {
                r#"[{"id":1,"title":"Song","hasFile":true,"trackFileId":8},{"id":2,"title":"Alternate release bonus","hasFile":false,"trackFileId":0}]"#
            }
            "/api/v1/trackFile" => r#"[{"id":8,"path":"/data/music/Artist/Album/01.flac","size":456}]"#,
            _ => "[]",
        }
        .to_string()
    })
    .await;

    let state = build(&server).get_album_import_state(42).await.expect("a state");

    assert!(state.is_complete());
    assert_eq!(state.track_count, 1);
    assert_eq!(state.tracks.len(), 2);
}

#[test]
fn imported_paths_translate_relative_to_selected_root_and_reject_escapes() {
    let translated = translate_imported_path(
        "/data/music/Radiohead/In Rainbows/01.flac",
        Some("/data/music"),
        "/music",
    );
    assert_eq!(translated.as_deref(), Ok("/music/Radiohead/In Rainbows/01.flac"));
    assert!(matches!(
        translate_imported_path("/downloads/other.flac", Some("/data/music"), "/music"),
        Err(LidarrError::InvalidOperation(_))
    ));
}

// Beyond the C# file: what the client threw, by kind.

#[tokio::test]
async fn an_error_status_is_an_http_error_with_the_start_of_the_body() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401).set_body_string("x".repeat(400)))
        .mount(&server)
        .await;

    let error = build(&server).get_options().await.expect_err("401");
    assert_eq!(
        error,
        LidarrError::Http(format!("Lidarr returned HTTP 401: {}", "x".repeat(300)))
    );
    assert!(!build(&server).is_reachable().await);
}

#[tokio::test]
async fn nothing_is_asked_until_lidarr_is_set_up() {
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings::default())));
    assert_eq!(
        client.get_options().await,
        Err(LidarrError::InvalidOperation(
            "Lidarr URL and API key are required.".into()
        ))
    );
    assert!(!client.is_reachable().await);
    assert!(matches!(
        client.test_connection(" ", "key").await,
        Err(LidarrError::InvalidOperation(_))
    ));
}

#[tokio::test]
async fn adding_an_album_needs_the_root_folder_and_both_profiles() {
    let server = serve(|_, _| "[]".to_string()).await;
    let client = LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: LidarrSettings {
            base_url: Some(server.uri()),
            api_key: Some("k".into()),
            ..Default::default()
        },
        ..Default::default()
    })));
    assert_eq!(
        client
            .start_album_search(&candidate("mbid", "Album", "Artist", None))
            .await,
        Err(LidarrError::InvalidOperation(
            "Choose a Lidarr root folder, quality profile, and metadata profile.".into()
        ))
    );
    assert!(client.is_reachable().await);
}

#[tokio::test]
async fn a_lookup_with_no_single_match_is_an_invalid_operation() {
    let server = serve(|_, path| {
        if path == "/api/v1/album/lookup" {
            r#"[{"foreignAlbumId":"a","title":"In Rainbows","releaseDate":"2007-10-10","artist":{"artistName":"Radiohead"}},
                {"foreignAlbumId":"","title":"In Rainbows","artist":{"artistName":"Radiohead"}}]"#
        } else {
            "[]"
        }
        .to_string()
    })
    .await;
    let client = build(&server);

    let found = client
        .resolve_album("Radiohead", "In Rainbows", None)
        .await
        .expect("one album");
    assert_eq!(found.foreign_album_id, "a");
    assert_eq!(found.year, Some(2007));
    assert_eq!(
        client.resolve_album("Radiohead", "Kid A", None).await,
        Err(LidarrError::InvalidOperation(
            "Lidarr could not unambiguously match 'Radiohead - Kid A'.".into()
        ))
    );
    assert_eq!(
        client
            .resolve_album_by_foreign_id("A")
            .await
            .map(|c| c.map(|c| c.title)),
        Ok(Some("In Rainbows".to_string()))
    );
    let lookups: Vec<String> = requests(&server).await.iter().map(path_and_query).collect();
    assert!(lookups.contains(&"/api/v1/album/lookup?term=Radiohead%20In%20Rainbows".to_string()));
    assert!(lookups.contains(&"/api/v1/album/lookup?term=lidarr%3AA".to_string()));
}
