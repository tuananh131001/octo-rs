//! Port of `DownloadTaggingTests`: a download end to end, from the landed file to the tags on
//! disk and the report in the fetched-songs log. The catalog and the music database are the real
//! clients against one mock server, the loudness meter is a fake, the fingerprint service's
//! answer is a parsed lookup, and the files are real files in a temp folder.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octo_core::fingerprint::acoust_id_client::parse_lookup;
use octo_core::fingerprint::{AcoustIdLookup, VerificationResult, VerificationVerdict};
use octo_core::models::domain::Song;
use octo_core::models::download::DownloadHistoryEntry;
use octo_core::settings::{
    AppSettings, FolderStructure, MetadataSettings, SoulseekSettings, SubsonicSettings,
};
use octo_media::audio::{ILoudnessMeter, Loudness, ReplayGainTags};
use octo_media::tags::{TagField, TagFile, tag_writer_extras};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::placement_tests::{vorbis, vorbis_all};
use super::test_support::{Harness, build, catalog};
use super::*;
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;
use crate::services::test_support::{Routes, flac, mp3};

// ---- the fakes ----------------------------------------------------------------------------

struct FakeMeter {
    result: Option<Loudness>,
    called: AtomicBool,
    file_still_there_when_done: AtomicBool,
}

impl FakeMeter {
    fn new(result: Option<Loudness>) -> Arc<Self> {
        Arc::new(Self {
            result,
            called: AtomicBool::new(false),
            file_still_there_when_done: AtomicBool::new(false),
        })
    }

    fn called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ILoudnessMeter for FakeMeter {
    async fn measure(&self, path: &Path, _: i32, _: &CancellationToken) -> Option<Loudness> {
        self.called.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.file_still_there_when_done
            .store(path.is_file(), Ordering::SeqCst);
        self.result
    }
}

/// The C# test's `special` answer: the music database down, the catalog over its quota.
#[derive(Clone)]
struct DownAndThrottled;

impl Respond for DownAndThrottled {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if request.url.path().starts_with("/ws/2/") {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_string(
                r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#,
            )
        }
    }
}

struct Tagging {
    harness: Harness,
    meter: Arc<FakeMeter>,
    root: tempfile::TempDir,
    _server: MockServer,
}

impl Tagging {
    fn root(&self) -> &Path {
        self.root.path()
    }

    async fn download(&self, id: &str) -> anyhow::Result<String> {
        self.harness
            .service
            .download_song("test", id, &CancellationToken::new())
            .await
    }

    async fn download_in_walk(&self, id: &str, context: &Arc<AlbumTagContext<Loudness>>) -> String {
        self.harness
            .service
            .download_song_internal(
                "test",
                id,
                DownloadOptions {
                    trigger_album_download: false,
                    force_permanent: true,
                    suppress_notify: true,
                    album_context: Some(Arc::clone(context)),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .expect("downloaded")
    }

    fn last_entry(&self) -> DownloadHistoryEntry {
        let entries = self.harness.history.get_recent(1);
        assert_eq!(entries.len(), 1);
        entries.into_iter().next().expect("one entry")
    }
}

fn landed_flac(root: &Path, folder: &str, tag: impl FnOnce(&mut TagFile)) -> String {
    let dir = root.join(folder);
    std::fs::create_dir_all(&dir).expect("made");
    let path = dir.join(format!("{}.flac", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, flac()).expect("written");
    let mut file = TagFile::open(&path).expect("opens");
    file.set_title(Some("Teardrop"));
    file.set_performers(&["Massive Attack".to_string()]);
    tag(&mut file);
    file.save().expect("saved");
    path.to_string_lossy().into_owned()
}

struct Options {
    metadata: MetadataSettings,
    soulseek: SoulseekSettings,
    loudness: Option<Loudness>,
    layout: FolderStructure,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            metadata: MetadataSettings::default(),
            soulseek: SoulseekSettings::default(),
            loudness: Some(Loudness::new(-11.5, 6.3, -0.3)),
            layout: FolderStructure::Flat,
        }
    }
}

async fn build_with(songs: Vec<(&str, Song)>, server: MockServer, options: Options) -> Tagging {
    let root = tempfile::tempdir().expect("a temp dir");
    let meter = FakeMeter::new(options.loudness);
    let settings = AppSettings {
        subsonic: SubsonicSettings {
            folder_structure: options.layout,
            download_mode: octo_core::settings::DownloadMode::Track,
            storage_mode: octo_core::settings::StorageMode::Permanent,
            ..Default::default()
        },
        metadata: options.metadata,
        soulseek: options.soulseek,
        ..Default::default()
    };
    // The catalog reads its settings live; it shares the service's store below.
    let store = Arc::new(octo_core::settings::SettingsStore::for_tests(settings.clone()));
    let deezer = Arc::new(DeezerMetadataService::with_base_url(
        Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
        store,
        server.uri(),
    ));
    let music_brainz = Arc::new(MusicBrainzClient::with_base_url(
        Url::parse(&format!("{}/ws/2/", server.uri())).expect("a base address"),
    ));
    let services = DownloadServices {
        deezer: Some(deezer),
        music_brainz: Some(music_brainz),
        loudness_meter: Some(Arc::clone(&meter) as Arc<dyn ILoudnessMeter>),
        ..Default::default()
    };
    let harness = build(root.path(), settings, None, Some(catalog(songs)), services);
    Tagging {
        harness,
        meter,
        root,
        _server: server,
    }
}

async fn serve(routes: Vec<(&str, String)>) -> MockServer {
    Routes::new(
        routes
            .iter()
            .map(|(needle, body)| (*needle, vec![body.as_str()]))
            .collect(),
        crate::services::test_support::Matching::IgnoreCase,
    )
    .serve()
    .await
}

// ---- the fixtures -------------------------------------------------------------------------

const TEARDROP_LOOKUP: &str = r#"
{"status": "ok", "results": [{"id": "acoustid-1", "score": 0.97, "recordings": [{
  "id": "rec-teardrop", "title": "Teardrop", "duration": 330.2, "sources": 40, "isrcs": ["GBAAA9800001"],
  "artists": [{"id": "a-ma", "name": "Massive Attack", "joinphrase": " feat. "}, {"id": "a-ef", "name": "Elizabeth Fraser"}],
  "releasegroups": [
    {"id": "g-collected", "title": "Collected", "type": "Album", "secondarytypes": ["Compilation"],
     "artists": [{"id": "a-ma", "name": "Massive Attack"}],
     "releases": [{"id": "r-col", "date": {"year": 2006, "month": 3, "day": 27}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 14, "tracks": [{"id": "t-col", "position": 4}]}]}]},
    {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
     "artists": [{"id": "a-ma", "name": "Massive Attack"}],
     "releases": [
       {"id": "r-mezz-2019", "date": {"year": 2019, "month": 8, "day": 23}, "country": "XE",
        "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m19", "position": 3}]}]},
       {"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
        "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
  ]}]}]}
"#;

const MEZZANINE_DETAILS: &str = r#"
{"id": "r-mezz", "title": "Mezzanine", "status": "Official", "date": "1998-04-20", "country": "GB", "barcode": "724384559922",
 "label-info": [{"catalog-number": "CDV 2851", "label": {"id": "l-virgin", "name": "Virgin"}}],
 "release-group": {"id": "g-mezzanine", "title": "Mezzanine", "primary-type": "Album", "secondary-types": [], "first-release-date": "1998-04-20"},
 "artist-credit": [{"name": "Massive Attack", "artist": {"id": "a-ma", "name": "Massive Attack"}}],
 "media": [{"position": 1, "track-count": 11, "tracks": [
   {"id": "t-m", "position": 3, "number": "3", "title": "Teardrop", "recording": {"id": "rec-teardrop", "title": "Teardrop", "isrcs": ["GBAAA9800001"]}}]}],
 "genres": [{"name": "trip hop", "count": 9}, {"name": "electronic", "count": 3}, {"name": "downtempo", "count": 1}]}
"#;

fn mezzanine_routes(catalog_album: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "/album/1",
            r#"{"id":1,"record_type":"album","upc":"724384559922","label":"Virgin","release_date":"1998-04-20","nb_tracks":11,"artist":{"name":"Massive Attack"},"genres":{"data":[{"name":"Electro"}]}}"#
                .to_string(),
        ),
        (
            "/track/11",
            r#"{"id":11,"track_position":3,"disk_number":1,"isrc":"GBAAA9800001","gain":-9.8,"contributors":[{"name":"Massive Attack","role":"Main"},{"name":"Elizabeth Fraser","role":"Featured"}]}"#
                .to_string(),
        ),
        (
            "/search?q=",
            r#"{"data":[{"id":11,"title":"Teardrop","duration":330,"isrc":"GBAAA9800001","album":{"id":1,"title":"CATALOG_ALBUM","cover_xl":"https://cdn.example/mezz.jpg"},"artist":{"name":"Massive Attack"}}]}"#
                .replace("CATALOG_ALBUM", catalog_album),
        ),
        ("release/r-mezz?", MEZZANINE_DETAILS.to_string()),
    ]
}

fn lookup(json: &str) -> AcoustIdLookup {
    parse_lookup(&serde_json::from_str(json).expect("json")).expect("a lookup")
}

fn confirmed_teardrop() -> VerificationResult {
    let lookup = lookup(TEARDROP_LOOKUP);
    let recording = lookup.results[0].recordings[0].clone();
    VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        score: 0.97,
        recording_id: Some(recording.recording_id.clone()),
        matched_title: Some(recording.title.clone()),
        matched_artist: Some(recording.artist_credit()),
        matched_album: recording.album_title.clone(),
        matched_year: recording.year,
        r#match: Some(recording),
        lookup: Some(lookup),
        acoust_id: Some("acoustid-1".into()),
        ..Default::default()
    }
}

fn confirm(song: &mut Song, verdict: VerificationResult) {
    verdict.apply_tags_to(song);
    song.verification = Some(Box::new(verdict));
}

fn teardrop() -> Song {
    Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    }
}

// ---- A: the FLAC ends with the full tag set -----------------------------------------------

#[tokio::test]
async fn lone_star_flac_ends_with_the_full_release_set_replay_gain_and_a_report() {
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Mezzanine")).await,
        Options::default(),
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        // `file.Tag.Comment`: on a FLAC, the Vorbis COMMENT field.
        const COMMENT: TagField = TagField {
            id3_frame: None,
            id3_description: Some("COMMENT"),
            vorbis: "COMMENT",
            mp4: "COMMENT",
        };
        landed_flac(&root, "peer share", |file| {
            tag_writer_extras::set_text(file, COMMENT, Some("a peer's comment"));
        })
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert_eq!(vorbis(&path, "DATE").as_deref(), Some("1998"));
    assert_eq!(vorbis(&path, "ORIGINALDATE").as_deref(), Some("1998-04-20"));
    assert_eq!(vorbis(&path, "LABEL").as_deref(), Some("Virgin"));
    assert_eq!(vorbis(&path, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&path, "BARCODE").as_deref(), Some("724384559922"));
    assert_eq!(vorbis(&path, "ISRC").as_deref(), Some("GBAAA9800001"));
    assert_eq!(vorbis(&path, "RELEASETYPE").as_deref(), Some("album"));
    assert_eq!(vorbis(&path, "RELEASESTATUS").as_deref(), Some("official"));
    assert_eq!(vorbis(&path, "RELEASECOUNTRY").as_deref(), Some("GB"));
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(),
        Some("rec-teardrop")
    );
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_RELEASETRACKID").as_deref(),
        Some("t-m")
    );
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_RELEASEGROUPID").as_deref(),
        Some("g-mezzanine")
    );
    assert_eq!(vorbis_all(&path, "MUSICBRAINZ_ARTISTID"), ["a-ma", "a-ef"]);
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_ALBUMARTISTID").as_deref(),
        Some("a-ma")
    );
    assert_eq!(vorbis(&path, "ACOUSTID_ID").as_deref(), Some("acoustid-1"));
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_TRACK_GAIN").as_deref(),
        Some("-6.50 dB")
    );
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_TRACK_PEAK").as_deref(),
        Some("0.966051")
    );
    assert_eq!(
        vorbis(&path, "TRACKNUMBER").and_then(|n| n.parse::<i32>().ok()),
        Some(3)
    );
    // The code has its own field now; the peer's comment is left exactly as it was.
    assert_eq!(vorbis(&path, "COMMENT").as_deref(), Some("a peer's comment"));

    let report = tagging.last_entry().tagging.expect("a report");
    assert_eq!(report.confidence, "Strong");
    assert_eq!(report.release_id.as_deref(), Some("r-mezz"));
    assert!(report.details_prefetch_hit);
    assert_eq!(report.fields["album"].source.as_deref(), Some("Fingerprint"));
    assert_eq!(report.integrated_lufs, Some(-11.5));
    assert!(report.stage_seconds.contains_key("loudness"));
    assert!(report.stage_seconds.contains_key("total"));
    // The measurement finished before the file moved: nothing reads a file while it moves.
    assert!(tagging.meter.called());
    assert!(tagging.meter.file_still_there_when_done.load(Ordering::SeqCst));
}

// ---- an upload's tags are not evidence ----------------------------------------------------

#[tokio::test]
async fn staged_upload_ignores_its_own_tags_as_evidence() {
    let routes = vec![
        (
            "/album/2",
            r#"{"id":2,"record_type":"single","release_date":"2021-05-01","nb_tracks":1,"artist":{"name":"Artist"}}"#
                .to_string(),
        ),
        ("/track/22", r#"{"id":22,"track_position":1,"disk_number":1}"#.to_string()),
        (
            "/search?q=",
            r#"{"data":[{"id":22,"title":"Song","duration":200,"album":{"id":2,"title":"Song"},"artist":{"name":"Artist"}}]}"#
                .to_string(),
        ),
    ];
    let song = Song {
        artist: "Artist".into(),
        title: "Song".into(),
        ..Default::default()
    };
    let tagging = build_with(vec![("1", song)], serve(routes).await, Options::default()).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |_, _| {
        let dir = root.join(INCOMING_FOLDER_NAME);
        std::fs::create_dir_all(&dir).expect("made");
        let path = dir.join("upload.mp3");
        std::fs::write(&path, mp3()).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_title(Some("Some Channel - Song"));
        file.set_album(Some("Some Channel"));
        file.set_performers(&["Some Channel".to_string()]);
        file.save().expect("saved");
        path.to_string_lossy().into_owned()
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        TagFile::open(&path).expect("opens").album().as_deref(),
        Some("Song")
    );
    let report = tagging.last_entry().tagging.expect("a report");
    assert!(!report.candidates.iter().any(|c| c.source == "FileTags"));
    assert_eq!(report.confidence, "Medium");
}

// ---- C: the request's album is never overwritten ------------------------------------------

#[tokio::test]
async fn album_walk_track_keeps_the_requested_album_and_track() {
    let song = Song {
        album: "Collected".into(),
        track: Some(4),
        total_tracks: Some(14),
        ..teardrop()
    };
    let tagging = build_with(
        vec![("1", song)],
        serve(mezzanine_routes("Mezzanine")).await,
        Options::default(),
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Collected"));
    assert_eq!(
        vorbis(&path, "TRACKNUMBER").and_then(|n| n.parse::<i32>().ok()),
        Some(4)
    );
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(),
        Some("rec-teardrop")
    );
    // The chooser confirmed the compilation, which is the release it was asked about.
    assert_eq!(
        tagging
            .last_entry()
            .tagging
            .expect("a report")
            .release_id
            .as_deref(),
        Some("r-col")
    );
}

// ---- G: a catalog-only Medium fills blanks only -------------------------------------------

#[tokio::test]
async fn catalog_only_medium_fills_blanks_only_no_release_facts_written() {
    let routes = vec![
        (
            "/album/2",
            r#"{"id":2,"record_type":"single","release_date":"2021-05-01","nb_tracks":1,"label":"A Label","upc":"111111111111","artist":{"name":"Artist"}}"#
                .to_string(),
        ),
        (
            "/track/22",
            r#"{"id":22,"track_position":1,"disk_number":1,"isrc":"USAAA2100001"}"#.to_string(),
        ),
        (
            "/search?q=",
            r#"{"data":[{"id":22,"title":"Song","duration":200,"album":{"id":2,"title":"Song"},"artist":{"name":"Artist"}}]}"#
                .to_string(),
        ),
    ];
    let song = Song {
        artist: "Artist".into(),
        title: "Song".into(),
        ..Default::default()
    };
    let tagging = build_with(vec![("1", song)], serve(routes).await, Options::default()).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |_, _| {
        landed_flac(&root, "peer share", |file| {
            file.set_title(Some("Song"));
            file.set_performers(&["Artist".to_string()]);
            file.set_album(Some("Peer Album"));
        })
    });

    let path = tagging.download("1").await.expect("downloaded");

    let report = tagging.last_entry().tagging.expect("a report");
    assert_eq!(report.confidence, "Medium");
    assert_eq!(report.source.as_deref(), Some("Catalog"));
    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Song"));
    assert_eq!(vorbis(&path, "DATE").as_deref(), Some("2021"));
    assert_eq!(vorbis(&path, "ISRC").as_deref(), Some("USAAA2100001"));
    // Only the chooser writes a release's kind and status, and it did not get to.
    assert_eq!(vorbis(&path, "RELEASETYPE"), None);
    assert_eq!(vorbis(&path, "RELEASESTATUS"), None);
    assert_eq!(vorbis(&path, "CATALOGNUMBER"), None);
    assert!(!report.fields.contains_key("album"));
}

// ---- H: the database down and the catalog throttled ---------------------------------------

#[tokio::test]
async fn database_down_and_catalog_throttled_keeps_the_fingerprints_release_and_says_so() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(DownAndThrottled)
        .mount(&server)
        .await;
    let tagging = build_with(vec![("1", teardrop())], server, Options::default()).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert_eq!(vorbis(&path, "DATE").as_deref(), Some("1998"));
    assert_eq!(
        vorbis(&path, "MUSICBRAINZ_TRACKID").as_deref(),
        Some("rec-teardrop")
    );
    assert_eq!(vorbis(&path, "LABEL"), None);
    assert_eq!(vorbis(&path, "CATALOGNUMBER"), None);
    let report = tagging.last_entry().tagging.expect("a report");
    assert_eq!(report.confidence, "Strong");
    assert!(report.notes.iter().any(|n| n.contains("catalog did not answer")));
    assert!(report.notes.iter().any(|n| n.contains("release lookup")));
    assert!(!report.candidates.iter().any(|c| c.source == "Catalog"));
}

// ---- I: a rip from a compilation ----------------------------------------------------------

const NOW_LOOKUP: &str = r#"
{"status": "ok", "results": [{"id": "acoustid-1", "score": 0.95, "recordings": [{
  "id": "rec-teardrop", "title": "Teardrop", "duration": 330, "sources": 40,
  "artists": [{"id": "a-ma", "name": "Massive Attack"}],
  "releasegroups": [
    {"id": "g-now42", "title": "Now That's What I Call Music! 42", "type": "Album", "secondarytypes": ["Compilation"],
     "artists": [{"id": "va", "name": "Various Artists"}],
     "releases": [{"id": "r-now", "date": {"year": 1999, "month": 4, "day": 12}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 21, "tracks": [{"id": "t-now", "position": 9}]}]}]},
    {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
     "artists": [{"id": "a-ma", "name": "Massive Attack"}],
     "releases": [{"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
       "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
  ]}]}]}
"#;

async fn compilation_rip(prefer_original_album: bool) -> Tagging {
    let routes = vec![
        ("release/r-mezz?", MEZZANINE_DETAILS.to_string()),
        ("/search?q=", r#"{"data":[]}"#.to_string()),
    ];
    let options = Options {
        metadata: MetadataSettings {
            prefer_original_album,
            ..Default::default()
        },
        ..Default::default()
    };
    let tagging = build_with(vec![("1", teardrop())], serve(routes).await, options).await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        let lookup = lookup(NOW_LOOKUP);
        let recording = lookup.results[0].recordings[0].clone();
        let verdict = VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            score: 0.95,
            recording_id: Some(recording.recording_id.clone()),
            matched_album: recording.album_title.clone(),
            r#match: Some(recording),
            lookup: Some(lookup),
            acoust_id: Some("acoustid-1".into()),
            ..Default::default()
        };
        confirm(song, verdict);
        landed_flac(&root, "peer share", |file| {
            file.set_album(Some("Now That's What I Call Music! 42"));
            file.set_album_artists(&["Various Artists".to_string()]);
            tag_writer_extras::set_compilation(file, true);
        })
    });
    tagging
}

#[tokio::test]
async fn compilation_rip_prefer_original_album_on_is_filed_under_the_studio_album() {
    let tagging = compilation_rip(true).await;

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Mezzanine"));
    assert_eq!(vorbis(&path, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&path, "COMPILATION"), None);
    assert_eq!(
        tagging.last_entry().tagging.expect("a report").confidence,
        "Medium"
    );
}

#[tokio::test]
async fn compilation_rip_prefer_original_album_off_keeps_the_compilation() {
    let tagging = compilation_rip(false).await;

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(
        vorbis(&path, "ALBUM").as_deref(),
        Some("Now That's What I Call Music! 42")
    );
    assert_eq!(vorbis(&path, "COMPILATION").as_deref(), Some("1"));
    assert_eq!(
        tagging
            .last_entry()
            .tagging
            .expect("a report")
            .release_id
            .as_deref(),
        Some("r-now")
    );
}

// ---- rehearsal ----------------------------------------------------------------------------

#[tokio::test]
async fn rehearsal_writes_todays_set_plus_the_additive_facts_and_reports_the_plan() {
    let options = Options {
        metadata: MetadataSettings {
            tag_rehearsal: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Collected")).await,
        options,
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    // What the old rules wrote: the catalog's own album for a song that arrived without one.
    assert_eq!(vorbis(&path, "ALBUM").as_deref(), Some("Collected"));
    assert_eq!(vorbis(&path, "CATALOGNUMBER"), None);
    assert_eq!(vorbis(&path, "MUSICBRAINZ_RELEASETRACKID"), None);
    assert_eq!(vorbis(&path, "RELEASESTATUS"), None);
    // The additive facts that do not depend on the match.
    assert_eq!(
        vorbis(&path, "REPLAYGAIN_TRACK_GAIN").as_deref(),
        Some("-6.50 dB")
    );
    assert_eq!(vorbis(&path, "ISRC").as_deref(), Some("GBAAA9800001"));
    assert_eq!(vorbis(&path, "ACOUSTID_ID").as_deref(), Some("acoustid-1"));
    let report = tagging.last_entry().tagging.expect("a report");
    assert!(report.rehearsed);
    assert_eq!(report.confidence, "Strong");
    assert_eq!(report.release_title.as_deref(), Some("Mezzanine"));
}

// ---- loudness that could not be measured --------------------------------------------------

#[tokio::test]
async fn measurement_that_gives_nothing_leaves_no_replay_gain_and_the_report_says_so() {
    let options = Options {
        loudness: Some(Loudness::new(f64::NEG_INFINITY, 0.0, f64::NEG_INFINITY)),
        ..Default::default()
    };
    let tagging = build_with(
        vec![("1", teardrop())],
        serve(mezzanine_routes("Mezzanine")).await,
        options,
    )
    .await;
    let root = tagging.root().to_path_buf();
    tagging.harness.backend.land(move |song, _| {
        confirm(song, confirmed_teardrop());
        landed_flac(&root, "peer share", |_| {})
    });

    let path = tagging.download("1").await.expect("downloaded");

    assert_eq!(vorbis(&path, "REPLAYGAIN_TRACK_GAIN"), None);
    assert!(
        tagging
            .last_entry()
            .tagging
            .expect("a report")
            .notes
            .iter()
            .any(|n| n.contains("loudness could not be measured"))
    );
}

// ---- two tracks of one walk share the album-level fields ----------------------------------

#[tokio::test]
async fn two_tracks_of_one_walk_share_the_release_facts() {
    let songs = vec![
        (
            "1",
            Song {
                album: "Mezzanine".into(),
                track: Some(3),
                ..teardrop()
            },
        ),
        (
            "2",
            Song {
                title: "Angel".into(),
                album: "Mezzanine".into(),
                track: Some(1),
                ..teardrop()
            },
        ),
    ];
    let options = Options {
        layout: FolderStructure::Organized,
        ..Default::default()
    };
    let tagging = build_with(songs, serve(mezzanine_routes("Mezzanine")).await, options).await;
    let context = Arc::new(AlbumTagContext::<Loudness>::new(
        Some("1"),
        "Mezzanine",
        Some("Massive Attack"),
    ));
    let root = tagging.root().to_path_buf();
    let second = Arc::new(AtomicBool::new(false));
    tagging.harness.backend.land(move |song, _| {
        let mut verdict = confirmed_teardrop();
        if second.load(Ordering::SeqCst) {
            // The second track's fingerprint names only the 2019 pressing of the same album.
            let lookup = lookup(
                &TEARDROP_LOOKUP
                    .replace("\"r-mezz\"", "\"r-gone\"")
                    .replace("1998", "2019"),
            );
            let mut matched = lookup.results[0].recordings[0].clone();
            matched.recording_id = "rec-angel".into();
            verdict = VerificationResult {
                lookup: Some(lookup),
                r#match: Some(matched),
                recording_id: Some("rec-angel".into()),
                ..verdict
            };
        }
        confirm(song, verdict);
        let title = song.title.clone();
        let path = landed_flac(&root, "peer share", |file| file.set_title(Some(&title)));
        second.store(true, Ordering::SeqCst);
        path
    });

    let first = tagging.download_in_walk("1", &context).await;
    let next = tagging.download_in_walk("2", &context).await;

    assert!(context.captured());
    assert_eq!(vorbis(&first, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&next, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(vorbis(&first, "DATE"), vorbis(&next, "DATE"));
    assert_eq!(vorbis(&first, "LABEL"), vorbis(&next, "LABEL"));
    assert_eq!(context.loudness().len(), 2);
    assert_eq!(Path::new(&first).parent(), Path::new(&next).parent());
}

#[tokio::test]
async fn write_album_gain_writes_the_album_values_into_every_measured_file() {
    let root = tempfile::tempdir().expect("a temp dir");
    let harness = build(
        root.path(),
        AppSettings::default(),
        None,
        None,
        DownloadServices::default(),
    );
    let context = AlbumTagContext::<Loudness>::new(Some("1"), "Mezzanine", Some("Massive Attack"));
    let a = landed_flac(root.path(), "album", |_| {});
    let b = landed_flac(root.path(), "album", |_| {});
    let (la, lb) = (Loudness::new(-10.0, 0.0, -1.0), Loudness::new(-14.0, 0.0, -0.5));
    context.set_loudness(&a, Some(la));
    context.set_loudness(&b, Some(lb));

    harness.service.write_album_gain(&context, "Mezzanine");

    let expected = ReplayGainTags::for_album(&[Some(la), Some(lb)]).expect("an album gain");
    assert_eq!(vorbis(&a, "REPLAYGAIN_ALBUM_GAIN"), Some(expected.gain_text()));
    assert_eq!(vorbis(&b, "REPLAYGAIN_ALBUM_GAIN"), Some(expected.gain_text()));
    assert_eq!(vorbis(&b, "REPLAYGAIN_ALBUM_PEAK"), Some(expected.peak_text()));
}

// ---- #69: a download with nothing on disk fails -------------------------------------------

#[tokio::test]
async fn a_landed_path_with_no_audio_fails_and_is_never_recorded() {
    for empty_file in [false, true] {
        let tagging = build_with(vec![("1", teardrop())], serve(vec![]).await, Options::default()).await;
        let path = tagging.root().join("slskd/incomplete/peer/Teardrop.flac");
        if empty_file {
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("made");
            std::fs::write(&path, []).expect("written");
        }
        let landed = path.to_string_lossy().into_owned();
        tagging.harness.backend.land(move |_, _| landed.clone());

        let error = tagging.download("1").await.expect_err("nothing on disk");

        assert!(
            error.downcast_ref::<FileNotFoundException>().is_some(),
            "{empty_file}: {error}"
        );
        assert!(tagging.harness.history.get_recent(50).is_empty(), "{empty_file}");
        assert!(!tagging.meter.called(), "{empty_file}");
    }
}
