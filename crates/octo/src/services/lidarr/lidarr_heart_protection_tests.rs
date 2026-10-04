//! Port of `octo.Tests/LidarrHeartProtectionTests.cs`.
//!
//! A heart through Lidarr keeps the promises a Soulseek heart does. What Lidarr imports goes
//! through the download pipeline song by song, so it meets the same checks; a song already in the
//! library is taken back out (an owned MP3 is queued for Better quality instead), a song removed
//! with a library action stays removed, and the album's monitoring goes back to how it was. The
//! heart learns whether its songs landed, so the next source can try what Lidarr could not get.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use octo_core::models::domain::{Album, Song};
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{
    AppSettings, DownloadSource, LibraryAction, LibraryActionDefinition, LibraryActionSettings,
    LidarrSettings, SettingsStore, SubsonicSettings,
};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

use super::*;
use crate::services::common::test_fakes::{FakeMetadata, until};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::library_ownership::Candidate;
use crate::services::library::{
    LibraryActionEntry, LibraryActionJournal, LibraryActionState, ReplacementHandoff,
};
use crate::services::lidarr::fake_lidarr::{ALBUM_ID, FakeLidarr};
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};

/// One file the pipeline took: its id, its song's title, and whether it landed quietly.
type Taken = Arc<Mutex<Vec<(String, String, bool)>>>;

/// The pipeline as far as Lidarr's files go: take the offered file, or refuse it the way
/// AcoustID refuses a wrong recording, which leaves it where Lidarr put it.
struct Pipeline {
    root: std::path::PathBuf,
    ids: Arc<ExternalIdRegistry>,
    imports: Arc<LidarrImportHandoff>,
    refused: Arc<Mutex<HashSet<String>>>,
    taken: Taken,
}

#[async_trait]
impl IDownloadService for Pipeline {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        provider: &str,
        id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        // The C# mock was set up for exactly these arguments.
        assert_eq!(
            (
                provider,
                trigger_album_download,
                force_permanent,
                source_override,
                upgrade_search
            ),
            ("soulseek", false, true, Some(DownloadSource::Lidarr), false)
        );
        assert!(replacement.is_none());
        let title = self
            .ids
            .lookup(id)
            .and_then(|r| r.snapshot().title)
            .unwrap_or_else(|| id.to_string());
        if self.refused.lock().contains(&title) {
            anyhow::bail!("AcoustID identified Lidarr's file as a live recording");
        }
        let import = self
            .imports
            .take(id)
            .ok_or_else(|| anyhow::anyhow!("nothing offered for {id}"))?;
        let placed = self.root.join(format!("Massive Attack - {title}.flac"));
        std::fs::rename(&import.path, &placed)?;
        self.taken.lock().push((id.to_string(), title, import.quiet));
        Ok(placed.to_string_lossy().into_owned())
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        anyhow::bail!("not set up")
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}

struct Fixture {
    root: TempDir,
    lidarr: FakeLidarr,
    server: MockServer,
    claims: Arc<LidarrAlbumClaims>,
    journal: Arc<LibraryActionJournal>,
    upgrades: Arc<UpgradeQueue>,
    imports: Arc<LidarrImportHandoff>,
    ids: Arc<ExternalIdRegistry>,
    refused: Arc<Mutex<HashSet<String>>>,
    taken: Taken,
    owned: Arc<Mutex<Vec<Candidate>>>,
}

fn upgrades_on() -> LibraryActionSettings {
    LibraryActionSettings {
        enabled: true,
        dry_run: false,
        allowed_users: vec!["alice".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::BetterQuality,
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn owned(id: &str, title: &str, suffix: &str, bit_rate: i32) -> Candidate {
    Candidate {
        id: id.into(),
        artist: "Massive Attack".into(),
        title: title.into(),
        album: Some("Mezzanine".into()),
        duration: Some(330),
        suffix: suffix.into(),
        bit_rate,
    }
}

impl Fixture {
    async fn new() -> Fixture {
        let root = tempfile::tempdir().expect("a temp dir");
        let (lidarr, server) = FakeLidarr::start(root.path()).await;
        Fixture {
            root,
            lidarr,
            server,
            claims: Arc::new(LidarrAlbumClaims::new()),
            journal: Arc::new(LibraryActionJournal::new()),
            upgrades: Arc::new(UpgradeQueue::new()),
            imports: Arc::new(LidarrImportHandoff::new()),
            ids: Arc::new(ExternalIdRegistry::in_memory()),
            refused: Arc::default(),
            taken: Arc::default(),
            owned: Arc::default(),
        }
    }

    fn id(&self, title: &str) -> String {
        self.ids.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some(title.into()),
            album: Some("Mezzanine".into()),
            ..Default::default()
        })
    }

    fn service(
        &self,
        actions: Option<LibraryActionSettings>,
        timeout_seconds: i32,
    ) -> LidarrHeartAcquisitionService {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            lidarr: LidarrSettings {
                base_url: Some(self.server.uri()),
                api_key: Some("k".into()),
                root_folder_path: Some("/data/music".into()),
                quality_profile_id: 1,
                metadata_profile_id: 1,
                import_timeout_seconds: timeout_seconds,
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some("http://navidrome.test".into()),
                auto_detect_download_path: false,
                skip_owned_songs: true,
                ..Default::default()
            },
            library_actions: actions.unwrap_or_default(),
            ..Default::default()
        }));
        settings.set_raw("Library:DownloadPath", Some(&self.root.path().to_string_lossy()));
        let identity = NavidromeIdentityService::new(settings.clone(), reqwest::Client::new());

        let metadata = Arc::new(FakeMetadata::default());
        let song = |title: &str, track: i32| Song {
            title: title.into(),
            artist: "Massive Attack".into(),
            album: "Mezzanine".into(),
            track: Some(track),
            external_provider: Some("soulseek".into()),
            external_id: Some(self.id(title)),
            ..Default::default()
        };
        // A track heart: Deezer found Teardrop on its single, where it is track 1.
        metadata
            .songs
            .lock()
            .insert(("soulseek".into(), "teardrop-heart".into()), song("Teardrop", 1));
        metadata.albums.lock().insert(
            ("soulseek".into(), "mezzanine".into()),
            Album {
                title: "Mezzanine".into(),
                artist: "Massive Attack".into(),
                songs: vec![song("Angel", 1), song("Teardrop", 3)],
                ..Default::default()
            },
        );

        let ownership = Arc::new(LibraryOwnership::new(
            settings.clone(),
            None,
            Arc::new(FakeLocalLibrary::default()),
        ));
        let library = self.owned.clone();
        ownership.set_search(Arc::new(move |_| {
            let candidates = library.lock().clone();
            async move { Ok(Some(candidates)) }.boxed()
        }));
        let dir = self.root.path().join("owned");
        ownership.set_resolve(Arc::new(move |id| {
            let dir = dir.clone();
            async move {
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{id}.file"));
                std::fs::write(&path, [1])?;
                Ok(Some(path.to_string_lossy().into_owned()))
            }
            .boxed()
        }));

        // Deezer answers nothing here, so a track heart files the song under its own album.
        let deezer = Arc::new(DeezerMetadataService::with_base_url(
            Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
            settings.clone(),
            "http://127.0.0.1:1",
        ));
        let downloads: Arc<dyn IDownloadService> = Arc::new(Pipeline {
            root: self.root.path().to_path_buf(),
            ids: self.ids.clone(),
            imports: self.imports.clone(),
            refused: self.refused.clone(),
            taken: self.taken.clone(),
        });
        let service = LidarrHeartAcquisitionService::new(
            Arc::new(LidarrClient::new(settings.clone())),
            metadata,
            deezer,
            settings.clone(),
            identity,
            Arc::new(NotificationService::new(Vec::new(), settings, None)),
            LidarrHeartExtras {
                claims: Some(self.claims.clone()),
                imports: Some(self.imports.clone()),
                ids: Some(self.ids.clone()),
                downloads: Some(downloads),
                journal: Some(self.journal.clone()),
                ownership: Some(ownership),
                upgrades: Some(self.upgrades.clone()),
                ..Default::default()
            },
        );
        service.set_poll_override(Some(Duration::from_millis(20)));
        service
    }

    async fn heart_the_album(&self, actions: Option<LibraryActionSettings>) -> bool {
        self.lidarr
            .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
        let service = self.service(actions, 30);
        within(service.try_acquire_album("soulseek", "mezzanine", true, Some("alice"))).await
    }

    fn taken(&self) -> Vec<String> {
        let mut titles: Vec<String> = self.taken.lock().iter().map(|t| t.1.clone()).collect();
        titles.sort();
        titles
    }

    fn quiet(&self, title: &str) -> bool {
        self.taken
            .lock()
            .iter()
            .find(|t| t.1 == title)
            .map(|t| t.2)
            .expect("taken")
    }

    /// The heart is answered as soon as its job ends; giving the album back to Lidarr's
    /// monitoring and letting go of the claim come right after, over HTTP. The C# test's
    /// in-memory handler finished those before the answer was read; here they are waited for.
    async fn job_wound_up(&self, unmonitored: bool) {
        until(|| !self.claims.heart_busy(ALBUM_ID)).await;
        if unmonitored {
            until(|| self.lidarr.monitor_changes().contains(&false)).await;
        }
    }
}

async fn within(work: impl Future<Output = anyhow::Result<bool>>) -> bool {
    tokio::time::timeout(Duration::from_secs(20), work)
        .await
        .expect("the heart is answered")
        .expect("a heart never throws")
}

#[tokio::test]
async fn every_song_goes_through_the_pipeline() {
    let f = Fixture::new().await;

    assert!(f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    assert!(f.lidarr.deleted().is_empty());
    // An album heart gets one notice for the album, so its songs land quietly.
    assert!(f.taken.lock().iter().all(|t| t.2));
    f.job_wound_up(true).await;
    assert!(f.lidarr.monitor_changes().contains(&false));
    assert!(!f.claims.heart_busy(ALBUM_ID));
}

#[tokio::test]
async fn a_song_already_in_the_library_is_taken_back_out() {
    let f = Fixture::new().await;
    *f.owned.lock() = vec![owned("nd-9", "Teardrop", "flac", 1000)];

    assert!(f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel"]);
    assert_eq!(f.lidarr.deleted().len(), 1);
    assert!(f.upgrades.snapshot().is_empty());
}

#[tokio::test]
async fn an_owned_mp3_is_queued_for_better_quality_instead() {
    let f = Fixture::new().await;
    *f.owned.lock() = vec![owned("nd-9", "Teardrop", "mp3", 320)];

    assert!(f.heart_the_album(Some(upgrades_on())).await);

    assert_eq!(f.taken(), ["Angel"]);
    assert_eq!(f.lidarr.deleted().len(), 1);
    let jobs = f.upgrades.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].navidrome_id, "nd-9");
    assert_eq!(jobs[0].requested_by, "alice");
}

#[tokio::test]
async fn a_removed_song_stays_removed() {
    let f = Fixture::new().await;
    f.journal.record(LibraryActionEntry {
        key: "delete:angel".into(),
        action: LibraryAction::Delete,
        navidrome_id: "nd-1".into(),
        username: "alice".into(),
        title: "Angel".into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
        source_path: None,
        quarantine_path: None,
        resolution: None,
        state: LibraryActionState::Applied,
        detail: Some("Removed.".into()),
        dry_run: false,
        at_utc: Utc::now(),
        history_kept: None,
        revealed_path: None,
    });

    // Removed means not wanted: the album is not short of anything the next source should fetch.
    assert!(!f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Teardrop"]);
    assert_eq!(f.lidarr.deleted().len(), 1);
}

#[tokio::test]
async fn a_song_the_pipeline_refuses_is_deleted_and_left_to_the_next_source() {
    let f = Fixture::new().await;
    f.refused.lock().insert("Teardrop".into());

    assert!(!f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel"]);
    // Refused before it was taken, so it is still where Lidarr put it, and goes through Lidarr.
    assert_eq!(f.lidarr.deleted().len(), 1);
}

/// A song heart: Lidarr still brings the whole album, the hearted song lands with its own
/// notice, the other song lands quietly, and Angel (track 1 on the album) is never taken
/// for Teardrop, which is track 1 on the single Deezer found it on.
#[tokio::test]
async fn a_song_heart_lands_its_song_with_a_notice_and_the_rest_quietly() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);

    let service = f.service(None, 30);
    assert!(within(service.try_acquire_track("soulseek", "teardrop-heart", true, Some("alice"))).await);

    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    assert!(!f.quiet("Teardrop"));
    assert!(f.quiet("Angel"));
    let id_of = |title: &str| {
        f.taken
            .lock()
            .iter()
            .find(|t| t.1 == title)
            .map(|t| t.0.clone())
            .expect("taken")
    };
    assert_eq!(id_of("Teardrop"), f.id("Teardrop"));
    assert_ne!(id_of("Angel"), f.id("Teardrop"));
}

#[tokio::test]
async fn nothing_arriving_in_time_is_a_miss_for_the_next_source() {
    let f = Fixture::new().await;

    let service = f.service(None, 1);
    let landed = within(service.try_acquire_album("soulseek", "mezzanine", true, Some("alice"))).await;

    assert!(!landed);
    assert!(f.taken.lock().is_empty());
    f.job_wound_up(true).await;
    assert!(!f.claims.heart_busy(ALBUM_ID));
}

#[tokio::test]
async fn a_second_heart_on_the_same_album_joins_and_is_answered_too() {
    let f = Fixture::new().await;
    let service = f.service(None, 30);

    let first = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album("soulseek", "mezzanine", true, Some("alice"))).await
        })
    };
    until(|| f.lidarr.searches() == 1).await;
    let second = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album("soulseek", "mezzanine", true, Some("bob"))).await
        })
    };
    // Bob has joined the running job before Lidarr brings anything (in the C#, the in-memory
    // handler had him there before the first await).
    until(|| service.askers_of(ALBUM_ID).is_some_and(|a| a.len() == 2)).await;
    // The release: Lidarr imports both songs.
    f.lidarr.import(1, "Angel", "FLAC");
    f.lidarr.import(3, "Teardrop", "FLAC");

    assert!(first.await.expect("the task"));
    assert!(second.await.expect("the task"));
    assert_eq!(f.lidarr.searches(), 1);
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

// ATrackHeartNeverMatchesByTrackNumber is in octo_core::lidarr::lidarr_heart_acquisition_service's
// tests, with match_song.

// Beyond the C# file.

/// An upgrade borrowing the album holds the heart back until it lets go.
#[tokio::test]
async fn a_heart_waits_for_an_upgrade_on_the_album() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.claims.upgrade_started(ALBUM_ID);
    let service = f.service(None, 30);
    service.set_claim_poll(Duration::from_millis(10));

    let heart = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album("soulseek", "mezzanine", true, Some("alice"))).await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.lidarr.searches(), 0);
    f.claims.upgrade_ended(ALBUM_ID);

    assert!(heart.await.expect("the task"));
    assert_eq!(f.lidarr.searches(), 1);
}

/// A heart Lidarr cannot place fails as itself, never as an error to the coordinator.
#[tokio::test]
async fn an_album_lidarr_does_not_know_is_a_plain_miss() {
    let f = Fixture::new().await;
    f.lidarr.set_knows_album(false);

    let service = f.service(None, 30);
    assert!(!within(service.try_acquire_album("soulseek", "mezzanine", true, Some("alice"))).await);
    assert!(!within(service.try_acquire_track("soulseek", "gone", true, None)).await);
    assert_eq!(f.lidarr.searches(), 0);
}

#[test]
fn relative_paths_follow_dotnet() {
    for (from, to, expected) in [
        ("/data/music", "/data/music", "."),
        ("/data/music/", "/data/music/a/b.flac", "a/b.flac"),
        ("/data/music", "/data/other/x", "../other/x"),
        ("/", "/a", "a"),
    ] {
        assert_eq!(get_relative_path(from, to), expected, "{from} -> {to}");
    }
    // A folder whose name starts with two dots is inside the root, but the escape check on
    // Octo's side reads it as a climb out, as the C# StartsWith("..") did.
    assert!(translate_imported_path("/data/music/..odd/x.flac", Some("/data/music"), "/music").is_err());
    assert_eq!(
        translate_imported_path("/data/music/x.flac", Some(" "), "/music"),
        Err(LidarrError::InvalidOperation(
            "Lidarr root folder is not configured.".into()
        ))
    );
}
