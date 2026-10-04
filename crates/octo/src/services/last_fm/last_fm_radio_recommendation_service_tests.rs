//! LastFmRadioRecommendationTests: the builds against a stand-in Last.fm answering by `method`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use octo_core::last_fm::last_fm_radio_recommendation_service::{RadioRandom, Randomizer, seeded_randomizer};
use octo_core::models::radio::LastFmRadioTrack;
use octo_core::settings::{AppSettings, DiscoveryStationSettings, LastFmSettings, MetadataSettings};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::soulseek::ExternalIdRegistry;

/// The C# `RecommendationHandler`.
#[derive(Clone)]
struct RecommendationHandler;

fn query(request: &Request) -> HashMap<String, String> {
    request.url.query_pairs().into_owned().collect()
}

impl Respond for RecommendationHandler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = query(request);
        let get = |name: &str| query.get(name).cloned().unwrap_or_default();
        let body = match get("method").as_str() {
            "artist.gettoptags" => json!({"toptags": {"tag": [
                {"name": "electronica"}, {"name": "seen live"}, {"name": "rock"}
            ]}}),
            "track.getsimilar" => json!({"similartracks": {"track": (0..15).map(|index| json!({
                "name": format!("similar-{index}"), "match": 1.0 - f64::from(index) / 100.0,
                "duration": 180000, "artist": {"name": format!("Similar Artist {index}")}
            })).collect::<Vec<_>>()}}),
            "artist.getsimilar" => json!({"similarartists": {"artist": (0..6).map(|index| json!({
                "name": format!("Neighbor {index}"), "match": 0.9 - f64::from(index) / 10.0
            })).collect::<Vec<_>>()}}),
            "artist.gettoptracks" => json!({"toptracks": {"track": (0..12).map(|index| json!({
                "name": format!("{}-top-{index}", get("artist"))
            })).collect::<Vec<_>>()}}),
            "tag.gettoptracks" => json!({"tracks": {"track": (0..20).map(|index| json!({
                "name": format!("{}-{index}", get("tag")), "duration": 180000,
                "artist": {"name": format!("{} Artist {index}", get("tag"))}
            })).collect::<Vec<_>>()}}),
            _ => json!({}),
        };
        ResponseTemplate::new(200).set_body_string(body.to_string())
    }
}

/// The C# `SeedSpecificHandler`: Last.fm fake whose similar tracks are specific to the seed asked
/// about.
#[derive(Clone)]
struct SeedSpecificHandler;

impl Respond for SeedSpecificHandler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = query(request);
        let seed = query.get("track").cloned().unwrap_or_else(|| "seed".into());
        let body: Value = match query.get("method").map(String::as_str) {
            Some("track.getsimilar") => json!({"similartracks": {"track": (0..15).map(|index| json!({
                "name": format!("{seed} sim {index}"), "match": 1.0 - f64::from(index) / 100.0,
                "duration": 180000, "artist": {"name": format!("{seed} Neighbour {index}")}
            })).collect::<Vec<_>>()}}),
            Some("artist.gettoptags") => json!({"toptags": {"tag": []}}),
            _ => json!({}),
        };
        ResponseTemplate::new(200).set_body_string(body.to_string())
    }
}

/// An empty answer to everything: the C# tests with no key called the real Last.fm, which
/// answered nothing usable.
#[derive(Clone)]
struct Nothing;

impl Respond for Nothing {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string("{}")
    }
}

struct Fixture {
    _dir: TempDir,
    _server: MockServer,
    settings: Arc<SettingsStore>,
    state: Arc<LastFmRadioStateStore>,
    service: LastFmRadioRecommendationService,
}

async fn fixture(last_fm: LastFmSettings, handler: impl Respond + 'static) -> Fixture {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(handler).mount(&server).await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm,
        metadata: MetadataSettings {
            language: "en".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let state = Arc::new(LastFmRadioStateStore::new(
        dir.path().join("state.json"),
        settings.clone(),
        Arc::new(ExternalIdRegistry::in_memory()),
    ));
    let client = Arc::new(LastFmService::with_base_url(
        settings.clone(),
        &format!("{}/", server.uri()),
    ));
    let service = LastFmRadioRecommendationService::new(client, state.clone(), settings.clone());
    Fixture {
        _dir: dir,
        _server: server,
        settings,
        state,
        service,
    }
}

fn record(state: &LastFmRadioStateStore, play: LastFmRadioPlay) {
    state.record_play("alice", play);
}

fn station(definition: (&str, &str, &[&str])) -> DiscoveryStationSettings {
    DiscoveryStationSettings {
        id: definition.0.into(),
        name: definition.1.into(),
        tags: definition.2.iter().map(|tag| tag.to_string()).collect(),
        ..Default::default()
    }
}

fn keys(station: &LastFmRadioStation) -> Vec<String> {
    station
        .tracks
        .iter()
        .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
        .collect()
}

async fn build(service: &LastFmRadioRecommendationService) -> Vec<LastFmRadioStation> {
    service
        .build("alice", &CancellationToken::new())
        .await
        .expect("the build succeeds")
}

/// A draw that always returns the same number: order becomes pure weight order.
fn flat_random() -> Randomizer {
    struct Flat;
    impl RadioRandom for Flat {
        fn next_double(&mut self) -> f64 {
            0.5
        }
    }
    Arc::new(|| Box::new(Flat) as Box<dyn RadioRandom>)
}

#[tokio::test]
async fn sparse_history_produces_deterministic_starter_and_local_pinned_fallback() {
    let fixture = fixture(
        LastFmSettings {
            api_key: String::new(),
            minimum_plays: 10,
            radio_track_count: 10,
            discovery_stations: vec![station(("rock", "Rock Discovery", &["rock"]))],
            ..Default::default()
        },
        Nothing,
    )
    .await;
    for index in 0..6 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("Artist {index}"),
                title: format!("Track {index}"),
                genre: Some(if index < 5 { "Rock" } else { "Jazz" }.into()),
                played_at_utc: Utc::now() - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    // Same seed for both builds: the draw is what varies a refresh, and with it pinned the rest
    // of the pipeline has to be deterministic.
    fixture.service.set_randomizer(seeded_randomizer(1234));
    let first = build(&fixture.service).await;
    let second = build(&fixture.service).await;
    assert!(first.iter().any(|s| s.kind == LastFmRadioStationKind::Starter));
    assert!(first.iter().any(|s| s.kind == LastFmRadioStationKind::Pinned));
    assert!(!first.iter().any(|s| s.kind == LastFmRadioStationKind::YourMix));
    let ids = |stations: &[LastFmRadioStation]| stations.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&first), ids(&second));
    let tracks = |stations: &[LastFmRadioStation]| {
        stations
            .iter()
            .flat_map(|s| s.tracks.iter().map(|t| format!("{}{}", t.artist, t.title)))
            .collect::<Vec<_>>()
    };
    assert_eq!(tracks(&first), tracks(&second));
}

#[tokio::test]
async fn learned_profile_applies_decay_aliases_denylist_discovery_merge_and_spacing() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 3,
            radio_track_count: 10,
            discovery_percent: 50,
            discovery_stations: vec![station(("fusion", "Fusion", &["rock", "idm"]))],
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    for index in 0..3 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: "Fresh".into(),
                title: format!("Fresh Seed {index}"),
                genre: Some("Electronica".into()),
                hearted: index == 0,
                played_at_utc: Utc::now() - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    for index in 0..9 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: "Repeated Old".into(),
                title: format!("Old Seed {index}"),
                genre: Some("Rock".into()),
                played_at_utc: Utc::now() - TimeDelta::days(80) + TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    fixture.state.reject_track(
        "alice",
        &LastFmRadioTrack {
            artist: "rock Artist 0".into(),
            title: "rock-0".into(),
            ..Default::default()
        },
        None,
    );

    let stations = build(&fixture.service).await;
    let mixes: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::YourMix)
        .collect();
    assert_eq!(mixes.len(), 1);
    let familiar_keys: Vec<String> = fixture
        .state
        .get_user("alice")
        .plays
        .iter()
        .map(|play| seed_normalizer::track_key(&play.artist, &play.title))
        .collect();
    let familiar = keys(mixes[0])
        .iter()
        .filter(|key| familiar_keys.contains(key))
        .count();
    assert!((1..=5).contains(&familiar), "{familiar} familiar tracks");

    let fresh: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::Artist && s.name == "Fresh Radio")
        .collect();
    assert_eq!(fresh.len(), 1);
    assert!(fresh[0].tracks.len() >= 5);
    assert!(
        stations
            .iter()
            .any(|s| s.kind == LastFmRadioStationKind::Genre
                && s.seeds.iter().any(|seed| seed == "electronic"))
    );
    assert!(
        !stations
            .iter()
            .flat_map(|s| &s.seeds)
            .any(|seed| seed == "seen live")
    );

    let pinned: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::Pinned)
        .collect();
    assert_eq!(pinned.len(), 1);
    assert!(pinned[0].tracks.iter().any(|t| t.title.starts_with("rock-")));
    assert!(pinned[0].tracks.iter().any(|t| t.title.starts_with("idm-")));
    assert!(!pinned[0].tracks.iter().any(|t| t.title == "rock-0"));
    assert_eq!(
        pinned[0].tracks.len() as i32,
        fixture.settings.current().last_fm.effective_radio_track_count()
    );
    for station in &stations {
        let mut distinct = keys(station);
        distinct.sort();
        distinct.dedup();
        assert_eq!(station.tracks.len(), distinct.len(), "{}", station.name);
        assert!(
            !station
                .tracks
                .windows(2)
                .any(|pair| dotnet::eq_ignore_case(&pair[0].artist, &pair[1].artist)),
            "{}",
            station.name
        );
    }
}

#[tokio::test]
async fn refresh_draws_a_different_snapshot_and_rotates_away_from_the_previous_one() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            enable_personalized_stations: false,
            discovery_stations: vec![station(("fusion", "Fusion", &["rock", "idm"]))],
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;

    // Two fresh builds from different draws over the same 40 candidates.
    fixture.service.set_randomizer(seeded_randomizer(1));
    let first = build(&fixture.service).await;
    assert_eq!(first.len(), 1);
    fixture.service.set_randomizer(seeded_randomizer(2));
    let alternative = build(&fixture.service).await;
    assert_eq!(alternative.len(), 1);
    assert_eq!(
        first[0].tracks.len() as i32,
        fixture.settings.current().last_fm.effective_radio_track_count()
    );
    assert_ne!(keys(&first[0]), keys(&alternative[0]));

    // Once the first snapshot is installed, a refresh mostly leaves it behind: ten of forty
    // candidates carry a third of their weight, so the expected carry-over is about one track.
    fixture.state.replace_stations("alice", &first);
    fixture.service.set_randomizer(seeded_randomizer(3));
    let refreshed = build(&fixture.service).await;
    assert_eq!(refreshed.len(), 1);
    let before = keys(&first[0]);
    let carried_over = keys(&refreshed[0])
        .iter()
        .filter(|key| before.contains(key))
        .count();
    assert!(carried_over <= 4, "{carried_over} carried over");
}

#[tokio::test]
async fn shaping_leads_with_the_provider_rank_and_pushes_the_previous_snapshot_behind_new_tracks() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            radio_track_count: 10,
            enable_personalized_stations: false,
            discovery_stations: vec![station(("fusion", "Fusion", &["rock", "idm"]))],
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    fixture.service.set_randomizer(flat_random());
    let rank = |track: &LastFmRadioTrack| -> i32 { track.title.rsplit('-').next().unwrap().parse().unwrap() };

    // Both tags answer rank 0..19 at match 1.0. With the draw flattened, the ten picks are the
    // five best-ranked of each tag, and rank 0 opens the station.
    let first = build(&fixture.service).await;
    assert_eq!(first.len(), 1);
    assert_eq!(rank(&first[0].tracks[0]), 0);
    assert!(first[0].tracks.iter().all(|track| (0..=4).contains(&rank(track))));

    // Installed as the previous snapshot, those ten keep a third of their weight, which is less
    // than any fresh candidate down to rank 19 carries. A refresh therefore continues down the
    // ranking instead of restating the top.
    fixture.state.replace_stations("alice", &first);
    let refreshed = build(&fixture.service).await;
    assert_eq!(refreshed.len(), 1);
    assert!(
        refreshed[0]
            .tracks
            .iter()
            .all(|track| (5..=9).contains(&rank(track)))
    );
}

#[tokio::test]
async fn radio_completions_count_less_than_chosen_plays_for_learning_and_seeding() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 5,
            radio_track_count: 10,
            enable_discovery_stations: false,
            ..Default::default()
        },
        RecommendationHandler,
    )
    .await;
    fixture.service.set_randomizer(seeded_randomizer(5));

    // Twelve tracks the radio played to the end weigh 4.8 against a threshold of five: not
    // enough to call the profile learned on the radio's own output.
    for index in 0..12 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("Radio Artist {index}"),
                title: format!("Served {index}"),
                source: "internet-radio".into(),
                played_at_utc: Utc::now() - TimeDelta::minutes(index),
                ..Default::default()
            },
        );
    }
    assert!(
        build(&fixture.service)
            .await
            .iter()
            .any(|s| s.kind == LastFmRadioStationKind::Starter)
    );

    // One play the listener chose tips it, and although it is the oldest play it is the
    // strongest seed, so the mix is seeded from it first.
    record(
        &fixture.state,
        LastFmRadioPlay {
            artist: "Chosen Artist".into(),
            title: "Chosen Track".into(),
            source: "scrobble".into(),
            played_at_utc: Utc::now() - TimeDelta::hours(2),
            ..Default::default()
        },
    );
    let stations = build(&fixture.service).await;
    let mixes: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::YourMix)
        .collect();
    assert_eq!(mixes.len(), 1);
    assert_eq!(mixes[0].seeds[0], "Chosen Artist");
}

#[tokio::test]
async fn mix_spreads_across_seeds_instead_of_letting_the_first_seed_fill_it() {
    let fixture = fixture(
        LastFmSettings {
            api_key: "key".into(),
            minimum_plays: 3,
            radio_track_count: 12,
            discovery_percent: 100,
            enable_discovery_stations: false,
            ..Default::default()
        },
        SeedSpecificHandler,
    )
    .await;
    for index in 0..6 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("Seed Artist {index}"),
                title: format!("Seed {index}"),
                played_at_utc: Utc::now() - TimeDelta::hours(index),
                ..Default::default()
            },
        );
    }
    fixture.service.set_randomizer(flat_random());

    // Every seed answers fifteen neighbours at the same match scores, and the newest seed
    // carries the most affinity. Without a share per source the flat draw would take the newest
    // seed's list and most of the second's; with it, no seed holds more than a share and a half
    // (three of twelve).
    let stations = build(&fixture.service).await;
    let mixes: Vec<_> = stations
        .iter()
        .filter(|s| s.kind == LastFmRadioStationKind::YourMix)
        .collect();
    assert_eq!(mixes.len(), 1);
    assert_eq!(mixes[0].tracks.len(), 12);
    let mut per_seed: HashMap<String, usize> = HashMap::new();
    for track in &mixes[0].tracks {
        *per_seed
            .entry(track.title.split(" sim ").next().unwrap().to_string())
            .or_insert(0) += 1;
    }
    assert!(
        per_seed.values().all(|count| (1..=3).contains(count)),
        "{per_seed:?}"
    );
    assert!(per_seed.len() >= 4, "only {} seeds represented", per_seed.len());
}

#[tokio::test]
async fn personalized_and_pinned_toggles_are_independent_and_do_not_delete_state() {
    let fixture = fixture(
        LastFmSettings {
            api_key: String::new(),
            enable_personalized_stations: false,
            enable_discovery_stations: true,
            discovery_stations: vec![station(("rock", "Rock", &["rock"]))],
            ..Default::default()
        },
        Nothing,
    )
    .await;
    for index in 0..3 {
        record(
            &fixture.state,
            LastFmRadioPlay {
                artist: format!("A{index}"),
                title: format!("T{index}"),
                genre: Some("rock".into()),
                ..Default::default()
            },
        );
    }
    let pinned_only = build(&fixture.service).await;
    assert!(
        pinned_only
            .iter()
            .all(|s| s.kind == LastFmRadioStationKind::Pinned)
    );

    let mut changed = fixture.settings.current().as_ref().clone();
    changed.last_fm = LastFmSettings {
        api_key: String::new(),
        enable_personalized_stations: true,
        enable_discovery_stations: false,
        discovery_stations: changed.last_fm.discovery_stations.clone(),
        ..Default::default()
    };
    fixture.settings.set(changed);
    let personalized_only = build(&fixture.service).await;
    assert!(!personalized_only.is_empty());
    assert!(
        !personalized_only
            .iter()
            .any(|s| s.kind == LastFmRadioStationKind::Pinned)
    );
    assert_eq!(fixture.state.get_user("alice").plays.len(), 3);
}

/// Enough plays that the build takes the learned branch and builds all four kinds. The key has
/// to be set: without one LastFmService short-circuits every tag lookup, so the discovery and
/// genre stations have nothing to be built from.
fn learned() -> LastFmSettings {
    LastFmSettings {
        api_key: "test-key".into(),
        enable_personalized_stations: true,
        enable_discovery_stations: false,
        minimum_plays: 3,
        ..Default::default()
    }
}

fn record_twelve(state: &LastFmRadioStateStore) {
    for index in 0..12 {
        record(
            state,
            LastFmRadioPlay {
                artist: format!("Artist {}", index % 3),
                title: format!("T{index}"),
                genre: Some("rock".into()),
                ..Default::default()
            },
        );
    }
}

/// Each dynamic station type is configured on its own (issue #39). Before this the four types
/// shared one switch and the counts were literals in BuildAsync, so a listener who wanted Your
/// Mix also got an artist radio per favorite band.
#[tokio::test]
async fn station_type_toggles_build_only_the_enabled_kinds() {
    let fixture = fixture(learned(), RecommendationHandler).await;
    record_twelve(&fixture.state);

    let all = build(&fixture.service).await;
    assert!(all.iter().any(|s| s.kind == LastFmRadioStationKind::Artist));
    assert!(all.iter().any(|s| s.kind == LastFmRadioStationKind::Genre));

    fixture.settings.set(AppSettings {
        last_fm: LastFmSettings {
            enable_your_mix: false,
            artist_station_count: 0,
            genre_station_count: 0,
            ..learned()
        },
        ..fixture.settings.current().as_ref().clone()
    });

    let discovery_only = build(&fixture.service).await;
    assert!(!discovery_only.iter().any(|s| matches!(
        s.kind,
        LastFmRadioStationKind::Artist
            | LastFmRadioStationKind::Genre
            | LastFmRadioStationKind::YourMix
            | LastFmRadioStationKind::Starter
    )));
}

#[tokio::test]
async fn artist_station_count_decides_how_many_artist_radios_are_built() {
    let fixture = fixture(
        LastFmSettings {
            artist_station_count: 1,
            genre_station_count: 0,
            ..learned()
        },
        RecommendationHandler,
    )
    .await;
    record_twelve(&fixture.state);

    let stations = build(&fixture.service).await;
    assert_eq!(
        stations
            .iter()
            .filter(|s| s.kind == LastFmRadioStationKind::Artist)
            .count(),
        1
    );
}

/// Rust-only: switching radio off builds nothing, and a cancelled caller sees the cancellation
/// rather than a partial build.
#[tokio::test]
async fn radio_off_builds_nothing_and_a_cancelled_caller_is_told() {
    let fixture = fixture(
        LastFmSettings {
            enable_radio: false,
            ..learned()
        },
        RecommendationHandler,
    )
    .await;
    record_twelve(&fixture.state);
    assert!(build(&fixture.service).await.is_empty());

    fixture.settings.set(AppSettings {
        last_fm: learned(),
        ..Default::default()
    });
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = fixture
        .service
        .build("alice", &cancelled)
        .await
        .expect_err("the caller cancelled");
    assert!(error.is::<OperationCanceled>());
}
