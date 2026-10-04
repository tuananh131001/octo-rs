//! Port of `LocalLibraryServiceTests`, plus the `.mappings.json` contract (state-files.md §4.23).
//! The Moq'd `HttpMessageHandler` is a wiremock server answering every call with the scan status.

use std::sync::Arc;

use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::subsonic::NavidromeIdentityService;

const SCAN_STATUS: &str =
    r#"{"subsonic-response":{"status":"ok","scanStatus":{"scanning":false,"count":100}}}"#;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/rust-migration/fixtures/state/music/.mappings.json"
);

struct Fixture {
    dir: tempfile::TempDir,
    server: MockServer,
    service: LocalLibraryService,
}

impl Fixture {
    async fn new() -> Fixture {
        Self::answering(SCAN_STATUS).await
    }

    async fn answering(body: &str) -> Fixture {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("temp dir");
        let service = service_for(&server.uri(), dir.path());
        Fixture { dir, server, service }
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn mappings_file(&self) -> std::path::PathBuf {
        self.dir.path().join(".mappings.json")
    }
}

fn service_for(url: &str, download_path: &std::path::Path) -> LocalLibraryService {
    let store = SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    });
    store.set_raw("Library:DownloadPath", Some(&download_path.to_string_lossy()));
    let settings = Arc::new(store);
    let http = crate::services::http_client_factory::default_client();
    LocalLibraryService::new(
        settings.clone(),
        http.clone(),
        Arc::new(ExternalIdRegistry::in_memory()),
        NavidromeIdentityService::new(settings, http),
    )
}

fn song(id: &str, title: &str, provider: Option<&str>, external_id: Option<&str>) -> Song {
    Song {
        id: id.into(),
        title: title.into(),
        artist: "Test Artist".into(),
        album: "Test Album".into(),
        external_provider: provider.map(str::to_string),
        external_id: external_id.map(str::to_string),
        ..Default::default()
    }
}

#[tokio::test]
async fn parse_song_id_with_external_id_returns_correct_parts() {
    let f = Fixture::new().await;
    let (is_external, provider, external_id) = f.service.parse_song_id("ext-deezer-123456");
    assert!(is_external);
    assert_eq!(provider.as_deref(), Some("deezer"));
    assert_eq!(external_id.as_deref(), Some("123456"));
}

#[tokio::test]
async fn parse_song_id_with_local_id_returns_not_external() {
    let f = Fixture::new().await;
    assert_eq!(f.service.parse_song_id("local-789"), (false, None, None));
}

#[tokio::test]
async fn parse_song_id_with_numeric_id_returns_not_external() {
    let f = Fixture::new().await;
    assert_eq!(f.service.parse_song_id("12345"), (false, None, None));
}

#[tokio::test]
async fn get_local_path_for_external_song_when_not_registered_returns_null() {
    let f = Fixture::new().await;
    assert_eq!(
        f.service
            .get_local_path_for_external_song("deezer", "nonexistent")
            .await,
        None
    );
}

#[tokio::test]
async fn register_downloaded_song_then_get_local_path_returns_path() {
    let f = Fixture::new().await;
    let local_path = f.path("test-song.mp3");
    std::fs::write(&local_path, "fake audio content").expect("write");

    f.service
        .register_downloaded_song(
            &song("ext-deezer-123456", "Test Song", Some("deezer"), Some("123456")),
            &local_path,
        )
        .await
        .expect("registered");

    assert_eq!(
        f.service
            .get_local_path_for_external_song("deezer", "123456")
            .await,
        Some(local_path)
    );
}

#[tokio::test]
async fn get_local_path_for_external_song_when_file_deleted_returns_null() {
    let f = Fixture::new().await;
    let local_path = f.path("deleted-song.mp3");
    std::fs::write(&local_path, "fake audio content").expect("write");
    f.service
        .register_downloaded_song(
            &song(
                "ext-deezer-999999",
                "Deleted Song",
                Some("deezer"),
                Some("999999"),
            ),
            &local_path,
        )
        .await
        .expect("registered");
    std::fs::remove_file(&local_path).expect("delete");

    assert_eq!(
        f.service
            .get_local_path_for_external_song("deezer", "999999")
            .await,
        None
    );
}

#[tokio::test]
async fn register_downloaded_song_with_null_provider_does_nothing() {
    let f = Fixture::new().await;
    f.service
        .register_downloaded_song(
            &song("local-123", "Local Song", None, None),
            &f.path("local-song.mp3"),
        )
        .await
        .expect("does not fail");
    assert!(!f.mappings_file().exists());
}

#[tokio::test]
async fn trigger_library_scan_returns_true() {
    let f = Fixture::new().await;
    assert!(f.service.trigger_library_scan(false).await);
}

#[tokio::test]
async fn get_scan_status_returns_scan_status() {
    let f = Fixture::new().await;
    let status = f.service.get_scan_status().await.expect("a status");
    assert!(!status.scanning);
    assert_eq!(status.count, Some(100));
}

#[tokio::test]
async fn parse_song_id_various_inputs_returns_expected() {
    let f = Fixture::new().await;
    let cases: [(&str, bool, Option<&str>, Option<&str>); 8] = [
        ("ext-deezer-123", true, Some("deezer"), Some("123")),
        ("ext-spotify-abc123", true, Some("spotify"), Some("abc123")),
        ("ext-tidal-999-888", true, Some("tidal"), Some("999-888")),
        // New format - extracts numeric ID
        ("ext-deezer-song-123456", true, Some("deezer"), Some("123456")),
        ("123456", false, None, None),
        ("", false, None, None),
        ("ext-", false, None, None),
        ("ext-deezer", false, None, None),
    ];
    for (id, external, provider, external_id) in cases {
        let (is_external, p, e) = f.service.parse_song_id(id);
        assert_eq!(
            (is_external, p.as_deref(), e.as_deref()),
            (external, provider, external_id),
            "{id:?}"
        );
    }
}

#[tokio::test]
async fn parse_external_id_various_inputs_returns_expected() {
    let f = Fixture::new().await;
    type Case<'a> = (&'a str, bool, Option<&'a str>, Option<&'a str>, Option<&'a str>);
    let cases: [Case; 10] = [
        (
            "ext-deezer-song-123456",
            true,
            Some("deezer"),
            Some("song"),
            Some("123456"),
        ),
        (
            "ext-deezer-album-789012",
            true,
            Some("deezer"),
            Some("album"),
            Some("789012"),
        ),
        (
            "ext-deezer-artist-259",
            true,
            Some("deezer"),
            Some("artist"),
            Some("259"),
        ),
        (
            "ext-spotify-song-abc123",
            true,
            Some("spotify"),
            Some("song"),
            Some("abc123"),
        ),
        // Legacy format defaults to song
        ("ext-deezer-123", true, Some("deezer"), Some("song"), Some("123")),
        ("ext-tidal-999", true, Some("tidal"), Some("song"), Some("999")),
        ("123456", false, None, None, None),
        ("", false, None, None, None),
        ("ext-", false, None, None, None),
        ("ext-deezer", false, None, None, None),
    ];
    for (id, external, provider, kind, external_id) in cases {
        let (is_external, p, t, e) = f.service.parse_external_id(id);
        assert_eq!(
            (is_external, p.as_deref(), t.as_deref(), e.as_deref()),
            (external, provider, kind, external_id),
            "{id:?}"
        );
    }
}

// ---- Rust-only: the .mappings.json contract and the rest of the interface ------------------

/// The fixture is read and written back byte for byte, in its own key order.
#[tokio::test]
async fn the_mappings_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), &text).expect("copy the fixture");

    let mappings = f.service.get_mappings().await;
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0].artist, "Sigur Rós");
    assert_eq!(mappings[1].source_peer, None);

    // A forget of a path nobody has writes nothing; registering again writes the same file.
    assert!(!f.service.forget_mapping("/nowhere.flac").await.expect("no write"));
    {
        let mut cached = f.service.mappings.lock();
        let map = f.service.loaded(&mut cached).expect("loaded");
        f.service.save(map).expect("saved");
    }
    assert_eq!(
        std::fs::read_to_string(f.mappings_file()).expect("written"),
        text.trim_end_matches('\n')
    );
}

/// A forgotten path's slot is taken by the next download, as `Dictionary` reused it, and the
/// path matches ignoring case.
#[tokio::test]
async fn forget_mapping_drops_the_path_ignoring_case() {
    let f = Fixture::new().await;
    for (id, name) in [("1", "one.flac"), ("2", "two.flac"), ("3", "three.flac")] {
        f.service
            .register_downloaded_song(&song(id, name, Some("soulseek"), Some(id)), &f.path(name))
            .await
            .expect("registered");
    }

    assert!(
        f.service
            .forget_mapping(&f.path("TWO.FLAC"))
            .await
            .expect("written")
    );
    assert!(!f.service.forget_mapping("  ").await.expect("nothing"));
    f.service
        .register_downloaded_song(
            &song("4", "four", Some("soulseek"), Some("4")),
            &f.path("four.flac"),
        )
        .await
        .expect("registered");

    let written = std::fs::read_to_string(f.mappings_file()).expect("written");
    let keys: Vec<&str> = written
        .lines()
        .filter(|line| line.starts_with("  \""))
        .map(|line| line.trim().split('"').nth(1).unwrap_or_default())
        .collect();
    assert_eq!(keys, ["soulseek:1", "soulseek:4", "soulseek:3"]);
}

/// One match by tags, however they are written; none when two files match, or when the file
/// is gone; the album only narrows when both sides have one.
#[tokio::test]
async fn find_mapping_by_tags_answers_only_an_unambiguous_file() {
    let f = Fixture::new().await;
    let first = f.path("Too Good.flac");
    std::fs::write(&first, [1]).expect("write");
    let mut too_good = song("1", "Too Good (feat. Rihanna)", Some("soulseek"), Some("1"));
    too_good.artist = "Drake".into();
    too_good.album = "Views".into();
    f.service
        .register_downloaded_song(&too_good, &first)
        .await
        .expect("registered");

    let found = f
        .service
        .find_mapping_by_tags(Some("Drake feat. Rihanna"), Some("Too Good"), None)
        .await
        .expect("found");
    assert_eq!(found.local_path, first);
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good (feat. Rihanna)"), Some("Views"))
            .await
            .is_some()
    );
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good (feat. Rihanna)"), Some("Scorpion"))
            .await
            .is_none()
    );
    assert!(
        f.service
            .find_mapping_by_tags(None, Some("Too Good"), None)
            .await
            .is_none()
    );

    // A second copy under another id makes it ambiguous.
    let second = f.path("Too Good (2).flac");
    std::fs::write(&second, [1]).expect("write");
    f.service
        .register_downloaded_song(
            &song("2", "Too Good (feat. Rihanna)", Some("youtube"), Some("2")),
            &second,
        )
        .await
        .expect("registered");
    let mut by_drake = song("2", "Too Good (feat. Rihanna)", Some("youtube"), Some("2"));
    by_drake.artist = "Drake".into();
    f.service
        .register_downloaded_song(&by_drake, &second)
        .await
        .expect("registered");
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good"), None)
            .await
            .is_none()
    );
    std::fs::remove_file(&second).expect("delete");
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good"), None)
            .await
            .is_some()
    );
}

/// The decision recorded in known-diffs.md: a file that does not parse is moved aside, and
/// the service starts empty rather than failing every caller.
#[tokio::test]
async fn a_corrupt_mappings_file_is_moved_aside() {
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), "{ not json").expect("write");

    assert!(f.service.get_mappings().await.is_empty());
    assert!(!f.mappings_file().exists());
    let aside: Vec<String> = std::fs::read_dir(f.dir.path())
        .expect("list")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".mappings.json.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
}

/// A JSON `null` file is no mappings, as `?? new Dictionary` made it.
#[tokio::test]
async fn a_null_mappings_file_is_empty() {
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), "null").expect("write");
    assert!(f.service.get_mappings().await.is_empty());
    assert!(f.mappings_file().exists());
}

/// The second scan within 30 s is debounced (and reported as triggered); a forced one is not.
#[tokio::test]
async fn a_scan_soon_after_another_is_debounced_unless_forced() {
    let f = Fixture::new().await;
    assert!(f.service.trigger_library_scan(false).await);
    assert!(f.service.trigger_library_scan(false).await);
    assert_eq!(
        crate::services::test_support::received(&f.server)
            .await
            .iter()
            .filter(|r| r.url.path() == "/rest/startScan")
            .count(),
        1
    );
    assert!(f.service.trigger_library_scan(true).await);
    assert_eq!(
        crate::services::test_support::received(&f.server)
            .await
            .iter()
            .filter(|r| r.url.path() == "/rest/startScan")
            .count(),
        2
    );
}

/// A refused scan is false; a status of the wrong shape is none.
#[tokio::test]
async fn a_refused_scan_and_an_odd_status() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("temp dir");
    let service = service_for(&server.uri(), dir.path());
    assert!(!service.trigger_library_scan(true).await);
    assert!(service.get_scan_status().await.is_none());

    let odd = Fixture::answering(r#"{"subsonic-response":{"scanStatus":{"scanning":"no"}}}"#).await;
    assert!(odd.service.get_scan_status().await.is_none());
    let bare = Fixture::answering(r#"{"subsonic-response":{"status":"ok"}}"#).await;
    assert!(bare.service.get_scan_status().await.is_none());
}
