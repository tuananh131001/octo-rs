//! Rust-only: `ReleaseIdentifier` over fake sources. The C# drove it only through the download
//! base (`DownloadTaggingTests`, 4-B); these pin each stage, budget and note on their own.

use std::time::Duration;

use parking_lot::Mutex;

use super::*;
use crate::fingerprint::acoust_id_client::{
    AcoustIdCredit, AcoustIdRecording, AcoustIdRelease, AcoustIdResult,
};
use crate::fingerprint::verification::{VerificationResult, VerificationVerdict};
use crate::settings::AppSettings;

// ---- the fakes ------------------------------------------------------------------------

struct FakeFacts(FileFacts);

impl FileFactsReader for FakeFacts {
    fn read_facts(&self, _path: &str, tags_are_evidence: bool) -> FileFacts {
        FileFacts {
            tags_are_evidence,
            ..self.0.clone()
        }
    }
}

#[derive(Default)]
struct FakeDatabase {
    details: Option<Value>,
    isrc: Option<Value>,
    search: Option<Value>,
    delay: Duration,
    fail: bool,
    calls: Mutex<Vec<String>>,
}

impl FakeDatabase {
    async fn answer(&self, call: String, answer: Option<Value>) -> anyhow::Result<Option<Value>> {
        self.calls.lock().push(call);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            anyhow::bail!("503 Service Unavailable");
        }
        Ok(answer)
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl ReleaseLookup for FakeDatabase {
    async fn lookup_release(&self, release_id: &str) -> anyhow::Result<Option<ReleaseDetails>> {
        let doc = self
            .answer(format!("release:{release_id}"), self.details.clone())
            .await?;
        Ok(doc
            .and_then(|doc| ReleaseDetails::parse(&doc))
            .filter(|d| eq_ignore_case(&d.release_id, release_id)))
    }

    async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> anyhow::Result<Option<Value>> {
        self.answer(
            format!("search:{artist}|{title}|{duration_seconds}"),
            self.search.clone(),
        )
        .await
    }

    async fn lookup_isrc(&self, isrc: &str) -> anyhow::Result<Option<Value>> {
        self.answer(format!("isrc:{isrc}"), self.isrc.clone()).await
    }
}

#[derive(Default)]
struct FakeCatalog {
    answer: CatalogCandidates,
    delay: Duration,
    fail: bool,
}

#[async_trait]
impl CatalogLookup for FakeCatalog {
    async fn enrich_track_candidates(
        &self,
        _artist: &str,
        _title: &str,
        max: i32,
    ) -> anyhow::Result<CatalogCandidates> {
        assert_eq!(max, ReleaseIdentifier::CATALOG_CANDIDATES);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            anyhow::bail!("connection refused");
        }
        Ok(self.answer.clone())
    }
}

// ---- the fixtures ---------------------------------------------------------------------

const MEZZANINE_DETAILS: &str = r#"
{"id": "r-mezz", "title": "Mezzanine", "status": "Official", "date": "1998-04-20", "country": "GB", "barcode": "724384559922",
 "label-info": [{"catalog-number": "CDV 2851", "label": {"id": "l-virgin", "name": "Virgin"}}],
 "release-group": {"id": "g-mezzanine", "title": "Mezzanine", "primary-type": "Album", "secondary-types": [], "first-release-date": "1998-04-20"},
 "artist-credit": [{"name": "Massive Attack", "artist": {"id": "a-ma", "name": "Massive Attack"}}],
 "media": [{"position": 1, "track-count": 11, "tracks": [
   {"id": "t-m", "position": 3, "number": "3", "title": "Teardrop", "recording": {"id": "rec-teardrop", "title": "Teardrop", "isrcs": ["GBAAA9800001"]}}]}]}
"#;

const DISCOVERY_ISRC: &str = r#"
{"isrc": "GBDUW0000059", "recordings": [{
  "id": "rec-omt", "title": "One More Time", "length": 320000, "first-release-date": "2000-11-13",
  "artist-credit": [{"name": "Daft Punk", "artist": {"id": "a-dp", "name": "Daft Punk"}}],
  "releases": [
    {"id": "r-disc", "title": "Discovery", "status": "Official", "date": "2001-03-12", "country": "FR",
     "release-group": {"id": "g-disc", "title": "Discovery", "primary-type": "Album", "first-release-date": "2001-02-26"},
     "media": [{"position": 1, "track-count": 14, "track": [{"id": "t-omt", "position": 1}]}]}]}]}
"#;

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

fn release(id: &str, group: &str, title: &str, date: &str, secondary: &[&str]) -> AcoustIdRelease {
    AcoustIdRelease {
        release_id: Some(id.into()),
        release_group_id: Some(group.into()),
        title: Some(title.into()),
        group_title: Some(title.into()),
        primary_type: Some("Album".into()),
        secondary_types: secondary.iter().map(|s| s.to_string()).collect(),
        date: Some(date.into()),
        track_number: Some(3),
        track_count: Some(11),
        ..Default::default()
    }
}

/// What verification left on a confirmed Teardrop: the whole lookup.
fn teardrop_lookup() -> AcoustIdLookup {
    let recording = AcoustIdRecording {
        credits: vec![
            AcoustIdCredit::new("Massive Attack", Some("a-ma"), " feat. "),
            AcoustIdCredit::new("Elizabeth Fraser", Some("a-ef"), ""),
        ],
        duration_seconds: Some(330),
        isrcs: vec!["GBAAA9800001".into()],
        sources: 40,
        releases: vec![
            release(
                "r-col",
                "g-collected",
                "Collected",
                "2006-03-27",
                &["Compilation"],
            ),
            release("r-mezz-2019", "g-mezzanine", "Mezzanine", "2019-08-23", &[]),
            release("r-mezz", "g-mezzanine", "Mezzanine", "1998-04-20", &[]),
        ],
        ..AcoustIdRecording::new(
            "rec-teardrop",
            "Teardrop",
            ["Massive Attack", "Elizabeth Fraser"],
            None,
            None,
        )
    };
    AcoustIdLookup::new(
        true,
        None,
        vec![AcoustIdResult {
            id: Some("acoustid-1".into()),
            ..AcoustIdResult::new(0.97, vec![recording])
        }],
    )
}

fn teardrop_song() -> Song {
    Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        verification: Some(Box::new(VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            lookup: Some(teardrop_lookup()),
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn peer_flac() -> FileFacts {
    FileFacts {
        duration_seconds: 330,
        extension: ".flac".into(),
        title: Some("Teardrop".into()),
        artist: Some("Massive Attack".into()),
        ..Default::default()
    }
}

fn identifier(
    settings: AppSettings,
    database: Option<Arc<FakeDatabase>>,
    catalog: Option<Arc<FakeCatalog>>,
    file: FileFacts,
) -> ReleaseIdentifier {
    ReleaseIdentifier::new(
        Arc::new(SettingsStore::for_tests(settings)),
        Arc::new(FakeFacts(file)),
        database.map(|d| d as Arc<dyn ReleaseLookup>),
        catalog.map(|c| c as Arc<dyn CatalogLookup>),
    )
}

async fn identify(identifier: &ReleaseIdentifier, song: &Song) -> Result<TagPlan, Cancelled> {
    let request = ReleaseIdentifier::request_for(song, &song.artist, &song.title, None, None);
    identifier
        .identify::<()>(
            song,
            request,
            "/m/teardrop.flac",
            true,
            None,
            &CancellationToken::new(),
        )
        .await
}

fn chosen_release(plan: &TagPlan) -> Option<&str> {
    plan.chosen
        .as_ref()
        .and_then(|c| c.candidate.release_id.as_deref())
}

// ---- the stages ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fingerprinted_file_takes_the_prefetched_release_details() {
    let database = Arc::new(FakeDatabase {
        details: Some(json(MEZZANINE_DETAILS)),
        ..Default::default()
    });
    let identifier = identifier(AppSettings::default(), Some(database.clone()), None, peer_flac());

    let plan = identify(&identifier, &teardrop_song()).await.expect("identified");

    assert_eq!(plan.confidence, TagConfidence::Strong);
    assert_eq!(chosen_release(&plan), Some("r-mezz"));
    assert!(plan.details_prefetch_hit);
    let chosen = &plan.chosen.as_ref().unwrap().candidate;
    assert_eq!(chosen.label.as_deref(), Some("Virgin"));
    assert_eq!(chosen.catalog_number.as_deref(), Some("CDV 2851"));
    assert_eq!(chosen.release_track_id.as_deref(), Some("t-m"));
    assert!(plan.details().is_some());
    // The fingerprint named the recording, so the database is never searched by name.
    assert_eq!(database.calls(), ["release:r-mezz"]);
    assert_eq!(
        plan.stage_seconds.keys().collect::<Vec<_>>(),
        ["details", "identify"]
    );
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    assert_eq!(
        plan.evidence
            .as_ref()
            .map(|e| e.fingerprinted_recording_ids.len()),
        Some(1)
    );
}

#[tokio::test]
async fn release_lookups_off_never_ask_the_database() {
    let database = Arc::new(FakeDatabase {
        details: Some(json(MEZZANINE_DETAILS)),
        ..Default::default()
    });
    let mut settings = AppSettings::default();
    settings.metadata.release_details_lookup = false;
    let identifier = identifier(settings, Some(database.clone()), None, peer_flac());

    let plan = identify(&identifier, &teardrop_song()).await.expect("identified");

    assert_eq!(chosen_release(&plan), Some("r-mezz"));
    assert!(plan.details().is_none());
    assert!(database.calls().is_empty());
    assert_eq!(plan.stage_seconds.keys().collect::<Vec<_>>(), ["identify"]);
}

#[tokio::test]
async fn no_fingerprint_asks_the_database_by_code_and_not_again_by_name() {
    let database = Arc::new(FakeDatabase {
        isrc: Some(json(DISCOVERY_ISRC)),
        ..Default::default()
    });
    let identifier = identifier(
        AppSettings::default(),
        Some(database.clone()),
        None,
        FileFacts::unknown(".mp3"),
    );
    let song = Song {
        artist: "Daft Punk".into(),
        title: "One More Time".into(),
        isrc: Some("gb-duw-00-00059".into()),
        ..Default::default()
    };

    let plan = identify(&identifier, &song).await.expect("identified");

    assert_eq!(chosen_release(&plan), Some("r-disc"));
    assert_eq!(database.calls(), ["isrc:GBDUW0000059", "release:r-disc"]);
    assert!(!plan.details_prefetch_hit);
    assert_eq!(
        plan.notes,
        [
            "the music database did not answer the release lookup; label, catalogue number and barcode may be missing"
        ]
    );
    assert_eq!(
        plan.stage_seconds.keys().collect::<Vec<_>>(),
        ["database", "details", "identify"]
    );
}

#[tokio::test]
async fn an_unanswered_search_says_so() {
    let database = Arc::new(FakeDatabase::default());
    let identifier = identifier(AppSettings::default(), Some(database.clone()), None, peer_flac());
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    };

    let plan = identify(&identifier, &song).await.expect("identified");

    assert_eq!(database.calls(), ["search:Massive Attack|Teardrop|330"]);
    assert_eq!(plan.notes, ["the music database did not answer the search"]);
    assert_eq!(plan.confidence, TagConfidence::None);
}

#[tokio::test(start_paused = true)]
async fn a_slow_database_costs_its_candidates_and_says_so() {
    let database = Arc::new(FakeDatabase {
        isrc: Some(json(DISCOVERY_ISRC)),
        delay: Duration::from_secs(20),
        ..Default::default()
    });
    let identifier = identifier(
        AppSettings::default(),
        Some(database),
        None,
        FileFacts::unknown(".mp3"),
    );
    let song = Song {
        artist: "Daft Punk".into(),
        title: "One More Time".into(),
        isrc: Some("GBDUW0000059".into()),
        ..Default::default()
    };

    let plan = identify(&identifier, &song).await.expect("identified");

    assert_eq!(plan.notes, ["the music database did not answer in 15 s"]);
    assert_eq!(plan.confidence, TagConfidence::None);
    assert_eq!(plan.stage_seconds["database"].round(), 15.0);
}

#[tokio::test]
async fn a_failing_database_could_not_be_asked() {
    let database = Arc::new(FakeDatabase {
        fail: true,
        ..Default::default()
    });
    let identifier = identifier(AppSettings::default(), Some(database), None, peer_flac());
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    };

    let plan = identify(&identifier, &song).await.expect("identified");

    assert_eq!(plan.notes, ["the music database could not be asked"]);
}

#[tokio::test(start_paused = true)]
async fn a_slow_catalog_costs_its_candidates_and_says_so() {
    let catalog = Arc::new(FakeCatalog {
        answer: CatalogCandidates {
            hits: vec![FullTrackMeta {
                title: Some("Teardrop".into()),
                ..Default::default()
            }],
            did_not_answer: false,
        },
        delay: Duration::from_secs(30),
        ..Default::default()
    });
    let identifier = identifier(AppSettings::default(), None, Some(catalog), peer_flac());

    let plan = identify(&identifier, &teardrop_song()).await.expect("identified");

    assert_eq!(plan.notes, ["the catalog did not answer in 12 s"]);
    assert!(plan.catalog_best.is_none());
    assert_eq!(plan.stage_seconds["catalog"].round(), 12.0);
}

#[tokio::test]
async fn the_catalogs_hits_are_candidates_and_the_first_is_kept_for_the_blanks() {
    let hit = |title: &str| FullTrackMeta {
        title: Some(title.into()),
        artist_name: Some("Massive Attack".into()),
        album_title: Some("Mezzanine".into()),
        record_type: Some("album".into()),
        duration: Some(330),
        ..Default::default()
    };
    let catalog = Arc::new(FakeCatalog {
        answer: CatalogCandidates {
            hits: vec![hit("Teardrop"), hit("Angel")],
            did_not_answer: false,
        },
        ..Default::default()
    });
    let identifier = identifier(
        AppSettings::default(),
        None,
        Some(catalog),
        FileFacts::unknown(".mp3"),
    );
    let song = Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        ..Default::default()
    };

    let plan = identify(&identifier, &song).await.expect("identified");

    assert_eq!(plan.ranked.len(), 2);
    assert_eq!(
        plan.catalog_best.as_ref().and_then(|m| m.title.as_deref()),
        Some("Teardrop")
    );
    assert_eq!(
        plan.chosen.as_ref().map(|c| c.candidate.source),
        Some(TagSource::Catalog)
    );
    assert_eq!(
        plan.stage_seconds.keys().collect::<Vec<_>>(),
        ["catalog", "identify"]
    );

    let silent = identifier_with_catalog(FakeCatalog {
        answer: CatalogCandidates {
            hits: vec![],
            did_not_answer: true,
        },
        ..Default::default()
    });
    let plan = identify(&silent, &song).await.expect("identified");
    assert_eq!(plan.notes, ["the catalog did not answer"]);

    let failing = identifier_with_catalog(FakeCatalog {
        fail: true,
        ..Default::default()
    });
    let plan = identify(&failing, &song).await.expect("identified");
    assert_eq!(plan.notes, ["the catalog could not be asked"]);
}

fn identifier_with_catalog(catalog: FakeCatalog) -> ReleaseIdentifier {
    identifier(
        AppSettings::default(),
        None,
        Some(Arc::new(catalog)),
        FileFacts::unknown(".mp3"),
    )
}

#[tokio::test(start_paused = true)]
async fn a_slow_release_lookup_costs_its_facts_not_the_plan() {
    let database = Arc::new(FakeDatabase {
        details: Some(json(MEZZANINE_DETAILS)),
        delay: Duration::from_secs(11),
        ..Default::default()
    });
    let identifier = identifier(AppSettings::default(), Some(database), None, peer_flac());

    let plan = identify(&identifier, &teardrop_song()).await.expect("identified");

    assert_eq!(chosen_release(&plan), Some("r-mezz"));
    assert!(!plan.details_prefetch_hit);
    assert!(plan.details().is_none());
    assert_eq!(
        plan.notes,
        [
            "the music database did not answer the release lookup; label, catalogue number and barcode may be missing"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_the_release_lookup_is_an_error() {
    let database = Arc::new(FakeDatabase {
        details: Some(json(MEZZANINE_DETAILS)),
        delay: Duration::from_secs(5),
        ..Default::default()
    });
    let identifier = identifier(AppSettings::default(), Some(database), None, peer_flac());
    let song = teardrop_song();
    let ct = CancellationToken::new();
    let cancel = ct.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
    });

    let request = ReleaseIdentifier::request_for(&song, &song.artist, &song.title, None, None);
    let answer = identifier
        .identify::<()>(&song, request, "/m/teardrop.flac", true, None, &ct)
        .await;

    assert_eq!(answer, Err(Cancelled));
}

#[tokio::test]
async fn an_album_walk_keeps_its_release_over_another_pressing() {
    let database = Arc::new(FakeDatabase {
        details: Some(json(&MEZZANINE_DETAILS.replace("\"r-mezz\"", "\"r-mezz-2019\""))),
        ..Default::default()
    });
    let identifier = identifier(AppSettings::default(), Some(database.clone()), None, peer_flac());
    let context: AlbumTagContext = AlbumTagContext::new(None, "Mezzanine", Some("Massive Attack"));
    let first = TagPlan {
        confidence: TagConfidence::Strong,
        chosen: Some(crate::tagging::tag_plan::ScoredCandidate::new(
            ReleaseCandidate {
                release_id: Some("r-mezz-2019".into()),
                release_group_id: Some("g-mezzanine".into()),
                ..ReleaseCandidate::new(TagSource::Fingerprint, "Angel", "Massive Attack")
            },
            0.0,
            vec![],
        )),
        ..Default::default()
    };
    context.capture(
        &first,
        &Song {
            album: "Mezzanine".into(),
            ..Default::default()
        },
    );
    let song = teardrop_song();
    let request = ReleaseIdentifier::request_for(&song, &song.artist, &song.title, None, None);

    let plan = identifier
        .identify(
            &song,
            request,
            "/m/t.flac",
            true,
            Some(&context),
            &CancellationToken::new(),
        )
        .await
        .expect("identified");

    assert_eq!(chosen_release(&plan), Some("r-mezz-2019"));
    assert!(database.calls().contains(&"release:r-mezz-2019".to_string()));
    assert!(plan.details().is_some());
    assert!(!plan.details_prefetch_hit);
    assert!(
        plan.notes
            .iter()
            .any(|n| n.starts_with("the walk's release was kept")),
        "{:?}",
        plan.notes
    );
}

// ---- the statics ----------------------------------------------------------------------------

#[test]
fn request_for_captures_the_request_before_anything_corrects_it() {
    let song = Song {
        disc_number: Some(2),
        duration: Some(301),
        isrc: Some("us-rc1-76-07839".into()),
        album_id: Some("ext-deezer-album-123456".into()),
        external_id: Some("987".into()),
        ..Default::default()
    };
    let request = ReleaseIdentifier::request_for(
        &song,
        "Nirvana",
        "Smells Like Teen Spirit (Live)",
        Some(" "),
        Some(4),
    );
    assert_eq!(request.album, None);
    assert_eq!(request.track, Some(4));
    assert_eq!(request.disc, Some(2));
    assert_eq!(request.duration_seconds, Some(301));
    assert_eq!(request.isrc.as_deref(), Some("USRC17607839"));
    assert_eq!(request.catalog_album_id.as_deref(), Some("123456"));
    assert_eq!(request.catalog_track_id.as_deref(), Some("987"));
    assert!(request.version_markers.contains("live"));

    assert_eq!(
        ReleaseIdentifier::catalog_album_id_of(Some("abc-")).as_deref(),
        Some("abc-")
    );
    assert_eq!(
        ReleaseIdentifier::catalog_album_id_of(Some("123")).as_deref(),
        Some("123")
    );
    assert_eq!(ReleaseIdentifier::catalog_album_id_of(Some("")), None);
}

#[test]
fn fingerprinted_ids_are_the_recordings_at_or_above_the_threshold() {
    let lookup = AcoustIdLookup::new(
        true,
        None,
        vec![
            AcoustIdResult::new(
                0.85,
                vec![
                    AcoustIdRecording::new("rec-a", "t", ["a"], None, None),
                    AcoustIdRecording::new("", "t", ["a"], None, None),
                ],
            ),
            AcoustIdResult::new(
                0.84,
                vec![AcoustIdRecording::new("rec-b", "t", ["a"], None, None)],
            ),
        ],
    );
    let ids = ReleaseIdentifier::fingerprinted_ids(Some(&lookup), 0.85);
    assert_eq!(ids.iter().collect::<Vec<_>>(), ["rec-a"]);
    assert!(ids.contains("REC-A"));
    let refused = AcoustIdLookup {
        is_ok: false,
        ..lookup
    };
    assert!(ReleaseIdentifier::fingerprinted_ids(Some(&refused), 0.85).is_empty());
    assert!(ReleaseIdentifier::fingerprinted_ids(None, 0.85).is_empty());
}

#[test]
fn pre_score_prefers_a_plain_album_then_the_earliest_first_release_then_pressing() {
    let candidate = |id: &str, primary: &str, secondary: &[&str], first: &str, date: &str| ReleaseCandidate {
        release_id: Some(id.into()),
        primary_type: Some(primary.into()),
        secondary_types: secondary.iter().map(|s| s.to_string()).collect(),
        group_first_release_date: Some(first.into()),
        release_date: Some(date.into()),
        ..ReleaseCandidate::new(TagSource::Fingerprint, "t", "a")
    };
    let pool = vec![
        candidate("single", "Single", &[], "1990", "1990"),
        candidate("comp", "Album", &["Compilation"], "1980", "1980"),
        candidate("reissue", "album", &[], "1998-04-20", "2019-08-23"),
        candidate("first", "Album", &[], "1998-04-20", "1998-04-20"),
        ReleaseCandidate {
            source: TagSource::Database,
            ..candidate("database", "Album", &[], "1900", "1900")
        },
    ];
    assert_eq!(ReleaseIdentifier::pre_score(&pool).as_deref(), Some("first"));
    assert_eq!(
        ReleaseIdentifier::pre_score(&pool[..2]).as_deref(),
        Some("single")
    );
    assert_eq!(ReleaseIdentifier::pre_score(&[]), None);
}
