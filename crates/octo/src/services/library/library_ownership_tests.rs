//! Port of `LibraryOwnershipTests`: what counts as the same song, the decision, and the lookup.
//! The download-path half (the `BaseDownloadService` harness) is 4-B's; see test-map.md.

use std::sync::Arc;

use futures::FutureExt;
use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::local::LocalSongMapping;
use crate::services::local::test_support::FakeLocalLibrary;

pub(crate) struct Root(tempfile::TempDir);

impl Root {
    pub(crate) fn new() -> Root {
        Root(tempfile::tempdir().expect("temp dir"))
    }

    pub(crate) fn path(&self, name: &str) -> String {
        self.0.path().join(name).to_string_lossy().into_owned()
    }
}

fn c(id: &str, artist: &str, title: &str, seconds: Option<i32>) -> Candidate {
    Candidate {
        id: id.into(),
        artist: artist.into(),
        title: title.into(),
        album: None,
        duration: seconds,
        suffix: "flac".into(),
        bit_rate: 900,
    }
}

fn lossy(mut candidate: Candidate, suffix: &str, bit_rate: i32) -> Candidate {
    candidate.suffix = suffix.into();
    candidate.bit_rate = bit_rate;
    candidate
}

// ---- The same song ------------------------------------------------------------------------

#[test]
fn the_same_song_is_within_eight_seconds() {
    for (seconds, same) in [(203, true), (211, false)] {
        assert_eq!(
            LibraryOwnership::same_song(
                &c("1", "Drake", "Started From the Bottom", Some(seconds)),
                "Drake",
                "Started From the Bottom",
                Some(200),
                None
            ),
            same,
            "{seconds}"
        );
    }
}

#[test]
fn a_live_take_is_not_the_studio_song() {
    assert!(!LibraryOwnership::same_song(
        &c("1", "Drake", "Started From the Bottom (Live)", Some(200)),
        "Drake",
        "Started From the Bottom",
        Some(200),
        None
    ));
}

#[test]
fn a_feature_written_either_way_is_the_same_song() {
    assert!(LibraryOwnership::same_song(
        &c("1", "Drake feat. Rihanna", "Too Good", Some(263)),
        "Drake",
        "Too Good (feat. Rihanna)",
        Some(263),
        None
    ));
}

#[test]
fn without_a_length_only_the_same_album_counts() {
    let mut intro = c("1", "Drake", "Intro", None);
    intro.album = Some("Thank Me Later".into());
    assert!(LibraryOwnership::same_song(
        &intro,
        "Drake",
        "Intro",
        None,
        Some("Thank Me Later")
    ));
    assert!(!LibraryOwnership::same_song(
        &intro,
        "Drake",
        "Intro",
        None,
        Some("Take Care")
    ));
    assert!(!LibraryOwnership::same_song(&intro, "Drake", "Intro", None, None));
}

fn ownership(
    root: &Root,
    library: Option<Vec<Candidate>>,
    local: Option<FakeLocalLibrary>,
) -> LibraryOwnership {
    let ownership = LibraryOwnership::new(
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        None,
        Arc::new(local.unwrap_or_default()),
    );
    ownership.set_search(search_from(library));
    ownership.set_resolve(resolve_to_files(root));
    ownership
}

/// Every id resolves to `<root>/<id>.file`, written on the spot.
pub(crate) fn resolve_to_files(root: &Root) -> ResolveFn {
    let dir = root.0.path().to_path_buf();
    Arc::new(move |id| {
        let path = dir.join(format!("{id}.file"));
        async move {
            std::fs::write(&path, [1])?;
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        .boxed()
    })
}

#[tokio::test]
async fn the_lossless_copy_is_chosen_over_a_lossy_one() {
    let root = Root::new();
    let owned = ownership(
        &root,
        Some(vec![
            lossy(c("mp3", "A", "Song", Some(200)), "mp3", 320),
            c("flac", "A", "Song", Some(201)),
        ]),
        None,
    )
    .find(Some("A"), Some("Song"), Some(200), None)
    .await
    .expect("owned");
    assert_eq!(owned.navidrome_id.as_deref(), Some("flac"));
    assert!(owned.lossless);
}

#[tokio::test]
async fn a_copy_whose_file_cannot_be_found_is_not_owned() {
    let root = Root::new();
    let ownership = ownership(&root, Some(vec![c("gone", "A", "Song", Some(200))]), None);
    ownership.set_resolve(Arc::new(|_| async { Ok(None) }.boxed()));
    assert_eq!(
        ownership.find(Some("A"), Some("Song"), Some(200), None).await,
        None
    );
}

#[tokio::test]
async fn without_navidrome_octos_own_downloads_still_count() {
    let root = Root::new();
    let path = root.path("Song.flac");
    std::fs::write(&path, [1]).expect("write");
    let local = FakeLocalLibrary::with_mapping(
        "A",
        "Song",
        None,
        LocalSongMapping {
            local_path: path.clone(),
            ..Default::default()
        },
    );
    let owned = ownership(&root, None, Some(local))
        .find(Some("A"), Some("Song"), Some(200), None)
        .await
        .expect("owned");
    assert_eq!(owned.absolute_path, path);
    assert_eq!(owned.navidrome_id, None);
    assert!(owned.lossless);
    assert_eq!(owned.suffix, "flac");
}

#[tokio::test]
async fn a_navidrome_that_fails_never_holds_a_download_back() {
    let root = Root::new();
    let ownership = ownership(&root, Some(Vec::new()), None);
    ownership.set_search(Arc::new(|_| async { Err(anyhow::anyhow!("down")) }.boxed()));
    assert_eq!(
        ownership.find(Some("A"), Some("Song"), Some(200), None).await,
        None
    );
}

// ---- The decision ------------------------------------------------------------------------

fn copy(lossless: bool, id: Option<&str>) -> OwnedCopy {
    OwnedCopy {
        navidrome_id: id.map(str::to_string),
        absolute_path: "/music/x".into(),
        suffix: if lossless { "flac" } else { "mp3" }.into(),
        bit_rate: 320,
        lossless,
    }
}

#[test]
fn the_decision() {
    let cases = [
        (None, true, true, OwnedDecision::Download),
        (Some(true), true, true, OwnedDecision::KeepYours),
        (Some(false), true, true, OwnedDecision::KeepAndUpgrade),
        (Some(false), false, true, OwnedDecision::KeepYours),
        (Some(false), true, false, OwnedDecision::KeepYours),
    ];
    for (lossless, source_can_be_lossless, upgrade_allowed, expected) in cases {
        let owned = lossless.map(|l| copy(l, Some("nd-1")));
        assert_eq!(
            LibraryOwnership::decide(owned.as_ref(), source_can_be_lossless, upgrade_allowed),
            expected,
            "{lossless:?} {source_can_be_lossless} {upgrade_allowed}"
        );
    }
}

#[test]
fn a_lossy_copy_navidrome_does_not_know_is_kept_not_upgraded() {
    assert_eq!(
        LibraryOwnership::decide(Some(&copy(false, None)), true, true),
        OwnedDecision::KeepYours
    );
}

// ---- Rust-only ---------------------------------------------------------------------------

/// The answer is remembered for two minutes, and `forget` drops it.
#[tokio::test]
async fn an_answer_is_remembered_for_two_minutes() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = Root::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let ownership = ownership(&root, None, None);
    let counted = calls.clone();
    ownership.set_search(Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Ok(Some(Vec::new())) }.boxed()
    }));
    let now = Arc::new(Mutex::new(Utc::now()));
    let clock_now = now.clone();
    ownership.set_clock(Clock::new(move || *clock_now.lock()));

    assert_eq!(
        ownership.find(Some("A"), Some("Song"), Some(200), None).await,
        None
    );
    assert_eq!(
        ownership.find(Some("A"), Some("Song"), Some(200), None).await,
        None
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    *now.lock() += TimeDelta::minutes(2);
    ownership.find(Some("A"), Some("Song"), Some(200), None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    ownership.forget();
    ownership.find(Some("A"), Some("Song"), Some(200), None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    // Nothing to look for without an artist or a title.
    assert_eq!(ownership.find(Some(" "), Some("Song"), None, None).await, None);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

fn navidrome_ownership(url: &str, admin: bool) -> LibraryOwnership {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }));
    let http = crate::services::http_client_factory::default_client();
    let identity = NavidromeIdentityService::new(settings.clone(), http.clone());
    if admin {
        identity.capture_login(
            br#"{"token":"jwt","isAdmin":true,"username":"admin","subsonicToken":"tok","subsonicSalt":"salt"}"#,
        );
    }
    let local: Arc<dyn ILocalLibraryService> = Arc::new(FakeLocalLibrary::default());
    let resolver = Arc::new(NavidromeSongPathResolver::new(
        identity.clone(),
        local.clone(),
        http.clone(),
        settings.clone(),
    ));
    LibraryOwnership::new(
        settings,
        Some(OwnershipNavidrome {
            identity,
            http,
            resolver,
        }),
        local,
    )
}

/// The real search3: signed as the admin identity, its songs read, a copy verified by the
/// resolver seam.
#[tokio::test]
async fn navidromes_search3_is_read() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/search3"))
        .and(query_param("query", "Drake Hold On"))
        .and(query_param("u", "admin"))
        .and(query_param("t", "tok"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[
                {"id":"nd-1","artist":"Drake","title":"Hold On, We're Going Home","duration":227,"suffix":"mp3","bitRate":320},
                {"id":"nd-2","artist":"Drake","title":"Hold On","album":"NWTS","duration":201,"suffix":"flac","bitRate":900},
                {"id":"","artist":"Drake","title":"Hold On"}
            ]}}}"#,
        ))
        .mount(&server)
        .await;
    let root = Root::new();
    let ownership = navidrome_ownership(&format!("{}/", server.uri()), true);
    ownership.set_resolve(resolve_to_files(&root));

    let owned = ownership
        .find(Some("Drake"), Some("Hold On"), Some(200), None)
        .await
        .expect("owned");
    assert_eq!(owned.navidrome_id.as_deref(), Some("nd-2"));
    assert_eq!(owned.absolute_path, root.path("nd-2.file"));
    assert!(owned.lossless);
}

/// Without an admin identity Navidrome is not asked, and nothing is owned.
#[tokio::test]
async fn without_an_admin_identity_navidrome_is_not_asked() {
    let ownership = navidrome_ownership("http://127.0.0.1:1", false);
    assert_eq!(ownership.find(Some("A"), Some("Song"), None, None).await, None);
}

#[test]
fn the_extension_is_read_as_path_get_extension_read_it() {
    assert_eq!(get_extension("/music/A/Song.FLAC"), ".FLAC");
    assert_eq!(get_extension("/music/A.b/Song"), "");
    assert_eq!(get_extension("/music/Song."), "");
    assert_eq!(get_extension("/music/.hidden"), ".hidden");
}

#[test]
fn better_quality_needs_every_gate() {
    use octo_core::settings::LibraryActionDefinition;
    let on = LibraryActionSettings {
        enabled: true,
        dry_run: false,
        allowed_users: vec!["alice".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::BetterQuality,
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(LibraryOwnership::upgrade_allowed(&on, Some("ALICE")));
    assert!(!LibraryOwnership::upgrade_allowed(&on, Some("bob")));
    assert!(!LibraryOwnership::upgrade_allowed(&on, None));
    let rehearsing = LibraryActionSettings {
        dry_run: true,
        ..on.clone()
    };
    assert!(!LibraryOwnership::upgrade_allowed(&rehearsing, Some("alice")));
}
