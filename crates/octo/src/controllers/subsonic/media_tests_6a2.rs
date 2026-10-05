//! `stream` and `getCoverArt` through the app: `ExternalPlaybackTests`, `OutsideSongSignInTests`,
//! `CleanCoverClientTests` and `GeneratedPlaylistControllerTests.GetCoverArt_ForAMix_IsAnImage`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use indexmap::IndexMap;
use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, SubsonicSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::xml::XElement;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::helpers_6a2::is_first_byte_request;
use super::media::draws_its_own_marks_in;
use super::test_support_6a2::*;

/// Navidrome accepting every sign-in: these tests are about what plays, not who.
async fn ping_ok() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#))
        .mount(&server)
        .await;
    server
}

fn playback_settings(url: &str, wait_for_lossless: bool, download_on_play: bool) -> AppSettings {
    let mut settings = settings(url);
    settings.subsonic = SubsonicSettings {
        wait_for_lossless_on_play: wait_for_lossless,
        download_on_play,
        ..settings.subsonic
    };
    settings
}

// ---- ExternalPlaybackTests ---------------------------------------------------------------

#[tokio::test]
async fn external_playback_uses_you_tube_regardless_of_legacy_storage_mode() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::streaming("soulseek", "track-id", &[1, 2, 3], "audio/mp4"));
    let library = Arc::new(FakeLibrary::with_external("external-track", "soulseek", "track-id"));
    let state = state_with(playback_settings(&navidrome.uri(), false, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);

    let response = get(
        &app,
        "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), [1, 2, 3]);
    assert_eq!(
        downloads.stream_calls(),
        [("soulseek".to_string(), "track-id".to_string(), None)]
    );
    assert_eq!(*downloads.download_and_stream_calls.lock(), 0);
}

#[tokio::test]
async fn wait_for_lossless_still_acquires_before_playback_when_enabled() {
    let navidrome = ping_ok().await;
    let downloads = Arc::new(FakeStreams::default());
    let library = Arc::new(FakeLibrary::with_external("external-track", "soulseek", "track-id"));
    let state = state_with(playback_settings(&navidrome.uri(), true, false), |inner| {
        inner.download_service = downloads.clone();
        inner.local_library = library.clone();
    });
    let app = app(&state);
    let response = tokio::spawn(async move {
        get(
            &app,
            "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
        )
        .await
    });

    let queue = state.track_acquisition_queue.clone();
    let request = tokio::time::timeout(Duration::from_secs(30), queue.dequeue(&CancellationToken::new()))
        .await
        .expect("the stream queued an acquisition")
        .expect("a request");
    assert!(!request.is_star);
    assert!(!request.trigger_album_download);
    assert!(request.force_permanent);

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("octo-playback.flac");
    std::fs::write(&path, [4, 5, 6]).expect("written");
    request
        .completion
        .try_set_result(path.to_string_lossy().into_owned());
    queue.release(&request);

    let response = response.await.expect("the request ran");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), [4, 5, 6]);
    assert_eq!(response.header("content-type"), Some("audio/flac"));
    assert!(downloads.stream_calls().is_empty());
}

#[tokio::test]
async fn streaming_an_outside_song_queues_its_download_only_from_the_first_byte() {
    for (range, waiting) in [(None, 1), (Some("bytes=0-"), 1), (Some("bytes=4096-"), 0)] {
        let navidrome = ping_ok().await;
        let downloads = Arc::new(FakeStreams::streaming("soulseek", "track-id", &[1, 2, 3], "audio/mp4"));
        let library = Arc::new(FakeLibrary::with_external("external-track", "soulseek", "track-id"));
        let state = state_with(playback_settings(&navidrome.uri(), false, true), |inner| {
            inner.download_service = downloads.clone();
            inner.local_library = library.clone();
        });
        let headers: Vec<(&str, &str)> = range.map(|r| ("Range", r)).into_iter().collect();

        let response = send(
            &app(&state),
            Method::GET,
            "/rest/stream?id=external-track&f=json&u=alice&t=good&s=salt&v=1.16.1&c=test",
            &headers,
            Body::empty(),
        )
        .await;

        assert_eq!(response.status, StatusCode::OK, "{range:?}");
        // The play is queued behind a look at slskd's login (the C# awaited it too, unawaited
        // by the request), so give it a moment.
        for _ in 0..100 {
            if state.track_acquisition_queue.waiting_plays() == waiting && waiting > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            state.track_acquisition_queue.waiting_plays(),
            waiting,
            "{range:?}"
        );
    }
}

#[test]
fn only_a_request_from_the_first_byte_is_a_play() {
    for (method, range, play) in [
        ("GET", None, true),
        ("GET", Some(""), true),
        ("GET", Some("bytes=0-"), true),
        ("GET", Some("bytes=0-1"), true),
        ("GET", Some("bytes=10-20"), false),
        ("HEAD", None, false),
    ] {
        assert_eq!(is_first_byte_request(method, range), play, "{method} {range:?}");
    }
}

// ---- OutsideSongSignInTests --------------------------------------------------------------

/// Navidrome as these calls need it: a ping that accepts the token "good" or the API key
/// "bob-key", tokenInfo for that key, and a library stream.
struct SignInNavidrome;

impl Respond for SignInNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let query = query_of(request);
        let path = request.url.path();
        if path.ends_with("/rest/ping") {
            let ok = query.get("t").map(String::as_str) == Some("good")
                || query.get("apiKey").map(String::as_str) == Some("bob-key");
            return navidrome_json(if ok { NAVIDROME_OK } else { NAVIDROME_WRONG_PASSWORD });
        }
        if path.ends_with("/rest/tokenInfo") {
            return navidrome_json(if query.get("apiKey").map(String::as_str) == Some("bob-key") {
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"bob"}}}"#
            } else {
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}"#
            });
        }
        if path.ends_with("/rest/stream") {
            return ResponseTemplate::new(200).set_body_raw(vec![7, 8, 9], "audio/flac");
        }
        ResponseTemplate::new(404)
    }
}

struct SignIn {
    navidrome: MockServer,
    state: crate::app::AppState,
    downloads: Arc<FakeStreams>,
}

impl SignIn {
    async fn new() -> SignIn {
        let navidrome = MockServer::start().await;
        Mock::given(any())
            .respond_with(SignInNavidrome)
            .mount(&navidrome)
            .await;
        let downloads = Arc::new(FakeStreams::streaming("soulseek", "track-id", &[1, 2, 3], "audio/mp4"));
        let library = Arc::new(FakeLibrary::with_external("external-track", "soulseek", "track-id"));
        let state = state_with(settings(&navidrome.uri()), |inner| {
            inner.download_service = downloads.clone();
            inner.local_library = library.clone();
        });
        SignIn {
            navidrome,
            state,
            downloads,
        }
    }

    async fn pings(&self) -> usize {
        received_paths(&self.navidrome)
            .await
            .iter()
            .filter(|p| p.ends_with("/rest/ping"))
            .count()
    }

    async fn streams(&self) -> usize {
        received_paths(&self.navidrome)
            .await
            .iter()
            .filter(|p| p.ends_with("/rest/stream"))
            .count()
    }

    /// Navidrome off: every call fails as a refused connection does.
    fn navidrome_down(&self, down: bool) {
        let url = if down {
            "http://127.0.0.1:1".to_string()
        } else {
            self.navidrome.uri()
        };
        self.state.settings.set(settings(&url));
    }
}

fn sign_in(token: &str) -> String {
    format!("u=alice&t={token}&s=salt&v=1.16.1&c=test")
}

fn json_error_code(reply: &Reply) -> i64 {
    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "failed", "{}", reply.text());
    envelope["error"]["code"].as_i64().expect("a code")
}

#[tokio::test]
async fn star_on_an_outside_song_with_a_wrong_password_starts_nothing() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        &format!("/rest/star.view?id=external-track&{}&f=json", sign_in("bad")),
    )
    .await;

    assert_eq!(json_error_code(&response), 40);
    assert!(fixture.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn star_with_no_sign_in_is_refused_without_a_ping() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        "/rest/star.view?id=external-track&u=alice&v=1.16.1&c=test&f=json",
    )
    .await;

    assert_eq!(json_error_code(&response), 40);
    assert_eq!(fixture.pings().await, 0);
    assert!(fixture.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn star_on_an_outside_album_with_a_wrong_password_starts_nothing() {
    let fixture = SignIn::new().await;
    let album_id = fixture.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("Massive Attack".into()),
        album: Some("Mezzanine".into()),
        ..Default::default()
    });

    let response = get(
        &app(&fixture.state),
        &format!("/rest/star.view?albumId={album_id}&{}&f=json", sign_in("bad")),
    )
    .await;

    assert_eq!(json_error_code(&response), 40);
    assert!(fixture.state.acquisition_tracker.all().is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Nothing was queued for the album: the acquisition queue holds no request.
    assert!(fixture.state.track_acquisition_queue.is_idle());
}

#[tokio::test]
async fn stream_of_an_outside_song_with_a_wrong_password_serves_nothing() {
    let fixture = SignIn::new().await;

    // No f: the error comes back in XML, as the client asked.
    let response = get(
        &app(&fixture.state),
        &format!("/rest/stream?id=external-track&{}", sign_in("bad")),
    )
    .await;

    let xml = XElement::parse(&response.text()).expect("XML");
    assert_eq!(xml.attribute("status"), Some("failed"));
    let error = xml.elements().find(|e| e.name == "error").expect("an error");
    assert_eq!(error.attribute("code"), Some("40"));
    assert!(fixture.downloads.stream_calls().is_empty());
}

#[tokio::test]
async fn stream_of_an_outside_song_same_address_twice_pings_once() {
    let fixture = SignIn::new().await;
    let app = app(&fixture.state);
    let url = format!("/rest/stream?id=external-track&{}&f=json", sign_in("good"));

    let first = get(&app, &url).await;
    let second = get(&app, &url).await;

    assert_eq!(first.body.as_ref(), [1, 2, 3]);
    assert_eq!(second.body.as_ref(), [1, 2, 3]);
    assert_eq!(fixture.pings().await, 1);
}

#[tokio::test]
async fn stream_while_navidrome_is_down_is_refused_and_recovers_at_once() {
    let fixture = SignIn::new().await;
    let app = app(&fixture.state);
    let url = format!("/rest/stream?id=external-track&{}&f=json", sign_in("good"));

    fixture.navidrome_down(true);
    let refused = get(&app, &url).await;
    assert_eq!(json_error_code(&refused), 0);
    assert!(fixture.downloads.stream_calls().is_empty());

    fixture.navidrome_down(false);
    let served = get(&app, &url).await;
    assert_eq!(served.body.as_ref(), [1, 2, 3]);
}

#[tokio::test]
async fn stream_of_a_library_song_is_left_to_navidrome() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        &format!("/rest/stream?id=library-track&{}", sign_in("good")),
    )
    .await;

    assert_eq!(response.body.as_ref(), [7, 8, 9]);
    assert_eq!(fixture.pings().await, 0);
    assert_eq!(fixture.streams().await, 1);
}

#[tokio::test]
async fn star_from_an_api_key_is_filed_under_the_keys_owner() {
    let fixture = SignIn::new().await;

    let response = get(
        &app(&fixture.state),
        "/rest/star.view?id=external-track&apiKey=bob-key&v=1.16.1&c=test&f=json",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(fixture.state.acquisition_tracker.for_user("bob").len(), 1);
}

// ---- CleanCoverClientTests ---------------------------------------------------------------

fn parameters(pairs: &[(&str, &str)]) -> octo_subsonic::Parameters {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn only_the_octo_app_gets_plain_covers() {
    for (client, plain) in [
        ("Octo", true),
        ("octo", true),
        (" Octo ", true),
        ("Symfonium", false),
        ("Feishin", false),
        ("OctoPlayer", false),
        ("", false),
    ] {
        assert_eq!(
            draws_its_own_marks_in(&parameters(&[("id", "abc"), ("c", client)])),
            plain,
            "{client:?}"
        );
    }
}

#[test]
fn a_request_without_a_client_name_keeps_the_badge() {
    assert!(!draws_its_own_marks_in(&parameters(&[("id", "abc")])));
}

// ---- GeneratedPlaylistControllerTests: getCoverArt ----------------------------------------

/// The C# `LibraryNavidrome`: a library with 40 Rock songs, and a cover for each.
struct LibraryNavidrome;

fn library_ok(inner: &str) -> String {
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1",{inner}}}}}"#)
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path().trim_matches('/').to_string();
        let songs = (0..40)
            .map(|i| {
                format!(
                    r#"{{"id":"lib{i}","title":"Track {i}","artist":"Artist {}","artistId":"ar{}","coverArt":"al-{}","duration":200,"playCount":5,"suffix":"flac","bitRate":900}}"#,
                    i % 20,
                    i % 20,
                    i % 5
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let body = match path.as_str() {
            "rest/getGenres" => {
                library_ok(r#""genres":{"genre":[{"value":"Rock","songCount":40},{"value":"Polka","songCount":3}]}"#)
            }
            "rest/getSongsByGenre" => library_ok(&format!(r#""songsByGenre":{{"song":[{songs}]}}"#)),
            "rest/getRandomSongs" => library_ok(r#""randomSongs":{"song":[]}"#),
            "rest/getCoverArt" => {
                let mut image = image::RgbImage::new(8, 8);
                for pixel in image.pixels_mut() {
                    *pixel = image::Rgb([200, 40, 40]);
                }
                let mut jpeg = Vec::new();
                image::DynamicImage::ImageRgb8(image)
                    .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
                    .expect("encoded");
                return ResponseTemplate::new(200).set_body_raw(jpeg, "image/jpeg");
            }
            _ => library_ok(r#""ping":{}"#),
        };
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/json")
    }
}

#[tokio::test]
async fn get_cover_art_for_a_mix_is_an_image() {
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(LibraryNavidrome)
        .mount(&navidrome)
        .await;
    let state = state_with(
        AppSettings {
            generated_playlists: GeneratedPlaylistSettings {
                enabled: true,
                ..Default::default()
            },
            ..settings(&navidrome.uri())
        },
        |_| {},
    );
    let auth: IndexMap<String, String> = [
        ("u", "alice"),
        ("t", "token"),
        ("s", "salt"),
        ("v", "1.16.1"),
        ("c", "test"),
        ("f", "json"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    // getPlaylists lists the mixes (6-A1's route); the service is what it asks.
    let mixes = state.generated_playlists.list("alice", &auth).await;
    let mix = mixes
        .iter()
        .find(|mix| mix.name == "Rock Mix")
        .unwrap_or_else(|| panic!("a Rock Mix in {:?}", mixes.iter().map(|m| &m.name).collect::<Vec<_>>()));

    let cover = get(
        &app(&state),
        &format!(
            "/rest/getCoverArt.view?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json&id={}",
            mix.id
        ),
    )
    .await;

    assert_eq!(cover.status, StatusCode::OK);
    assert_eq!(cover.media_type().as_deref(), Some("image/jpeg"));
    assert!(image::load_from_memory(&cover.body).is_ok(), "a picture");
}

/// Rust-only: the local relay, the octo-radio and placeholder answers, and a missing id, as
/// endpoints.md §3.4 lists them.
#[tokio::test]
async fn get_cover_art_answers_each_kind_of_id_as_the_csharp_did() {
    let navidrome = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/rest/getCoverArt"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1, 2, 3], "image/png"))
        .mount(&navidrome)
        .await;
    let state = state_with(settings(&navidrome.uri()), |_| {});
    let app = app(&state);

    let local = get(&app, "/rest/getCoverArt?id=al-1&u=a&t=good&s=s").await;
    assert_eq!(local.status, StatusCode::OK);
    assert_eq!(local.header("content-type"), Some("image/png"));
    assert_eq!(local.body.as_ref(), [1, 2, 3]);

    let missing = get(&app, "/rest/getCoverArt?u=a").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing.header("content-type"),
        Some("application/problem+json; charset=utf-8")
    );

    for id in ["octo-radio", "ext-album-unknown"] {
        let placeholder = get(&app, &format!("/rest/getCoverArt?id={id}")).await;
        assert_eq!(placeholder.status, StatusCode::OK, "{id}");
        assert_eq!(placeholder.header("content-type"), Some("image/jpeg"), "{id}");
        assert!(image::load_from_memory(&placeholder.body).is_ok(), "{id}");
    }
    // The Octo app draws its own tile for a cover Octo cannot find.
    let plain = get(&app, "/rest/getCoverArt?id=ext-album-unknown&c=Octo").await;
    assert_eq!(plain.status, StatusCode::NOT_FOUND);

    // A relay failure is the unbranded placeholder, still a 200.
    state.settings.set(settings("http://127.0.0.1:1"));
    let unreachable = get(&app, "/rest/getCoverArt?id=al-1").await;
    assert_eq!(unreachable.status, StatusCode::OK);
    assert_eq!(unreachable.header("content-type"), Some("image/jpeg"));
}

/// Rust-only: a local file served for an outside id under WaitForLosslessOnPlay honours one
/// byte range as ASP.NET's range processing did, and types the file by its extension.
#[tokio::test]
async fn a_lossless_copy_on_disk_is_served_with_ranges() {
    let navidrome = ping_ok().await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("song.m4a");
    std::fs::write(&path, (0u8..100).collect::<Vec<_>>()).expect("written");
    let library = Arc::new(FakeLibrary::with_external("external-track", "soulseek", "track-id"));
    library.local_paths.lock().insert(
        ("soulseek".into(), "track-id".into()),
        path.to_string_lossy().into_owned(),
    );
    let state = state_with(playback_settings(&navidrome.uri(), true, false), |inner| {
        inner.local_library = library.clone();
    });
    let app = app(&state);
    let url = "/rest/stream?id=external-track&u=alice&t=good&s=salt&v=1.16.1&c=test";

    let whole = get(&app, url).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.body.len(), 100);
    assert_eq!(whole.header("content-type"), Some("audio/mp4"));
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));

    let mut cases: HashMap<&str, (StatusCode, Option<&str>, usize)> = HashMap::new();
    cases.insert("bytes=10-19", (StatusCode::PARTIAL_CONTENT, Some("bytes 10-19/100"), 10));
    cases.insert("bytes=-5", (StatusCode::PARTIAL_CONTENT, Some("bytes 95-99/100"), 5));
    cases.insert("bytes=200-", (StatusCode::RANGE_NOT_SATISFIABLE, Some("bytes */100"), 0));
    for (range, (status, content_range, length)) in cases {
        let reply = send(&app, Method::GET, url, &[("Range", range)], Body::empty()).await;
        assert_eq!(reply.status, status, "{range}");
        assert_eq!(reply.header("content-range"), content_range, "{range}");
        assert_eq!(reply.body.len(), length, "{range}");
    }
}
