//! Port of `ExternalSearchServiceTests`. The Moq'd Last.fm handler is a wiremock server
//! answering by the `method` query parameter; the metadata service is a hand-written fake.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use octo_core::settings::{AppSettings, LastFmSettings, SubsonicSettings};
use tokio::sync::watch;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;

/// A Last.fm whose track.search answers `hits` rows ("Song 1".. by "Artist") and whose
/// artist.gettoptracks answers none.
async fn last_fm(hits: usize) -> (Arc<LastFmService>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let url = request.url.to_string();
            let tracks: Vec<String> = (1..=hits)
                .map(|i| format!(r#"{{"name":"Song {i}","artist":"Artist"}}"#))
                .collect();
            let body = if url.contains("method=track.search") {
                format!(
                    r#"{{"results":{{"trackmatches":{{"track":[{}]}}}}}}"#,
                    tracks.join(",")
                )
            } else {
                r#"{"toptracks":{"track":[]}}"#.to_string()
            };
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let service = Arc::new(LastFmService::with_base_url(
        settings,
        &format!("{}/", server.uri()),
    ));
    (service, server)
}

/// `OneHitLastFm`: every call answers one track.search row, Artist - Song.
async fn one_hit_last_fm() -> (Arc<LastFmService>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"results":{"trackmatches":{"track":[{"name":"Song","artist":"Artist"}]}}}"#,
            ),
        )
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    (
        Arc::new(LastFmService::with_base_url(
            settings,
            &format!("{}/", server.uri()),
        )),
        server,
    )
}

fn waiting(wait_for_search_durations: bool) -> Option<Arc<SettingsStore>> {
    Some(Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            wait_for_search_durations,
            ..Default::default()
        },
        ..Default::default()
    })))
}

fn one_song_metadata() -> Arc<FakeMetadata> {
    let metadata = Arc::new(FakeMetadata::default());
    metadata.hits.lock().insert(
        ("Artist".into(), "Song".into()),
        Song {
            artist: "Artist".into(),
            title: "Song".into(),
            ..Default::default()
        },
    );
    metadata
}

/// A gate the durations pass waits on, and the background flag it was called with.
struct DurationsGate {
    open: watch::Sender<bool>,
    started: watch::Receiver<Option<bool>>,
}

fn gate_durations(metadata: &FakeMetadata) -> DurationsGate {
    let (open, opened) = watch::channel(false);
    let (started_tx, started) = watch::channel(None);
    let started_tx = Arc::new(started_tx);
    *metadata.durations.lock() = Some(Arc::new(move |background| {
        started_tx.send_replace(Some(background));
        let mut opened = opened.clone();
        async move {
            let _ = opened.wait_for(|open| *open).await;
        }
        .boxed()
    }));
    DurationsGate { open, started }
}

#[tokio::test]
async fn pads_with_top_tracks_only_when_track_search_is_thin() {
    for (search_hits, expected_top_track_calls) in [(50, 0), (5, 1)] {
        let (last_fm, server) = last_fm(search_hits).await;
        let search = ExternalSearchService::new(Arc::new(FakeMetadata::default()), Some(last_fm), None);

        search.get("artist").await;

        let top_track_calls = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.to_string().contains("method=artist.gettoptracks"))
            .count();
        assert_eq!(top_track_calls, expected_top_track_calls, "{search_hits} hits");
    }
}

#[tokio::test]
async fn search_waits_for_you_tube_durations_only_when_configured() {
    for wait_for_durations in [true, false] {
        let metadata = one_song_metadata();
        let mut gate = gate_durations(&metadata);
        let (last_fm, _server) = one_hit_last_fm().await;
        let search = Arc::new(ExternalSearchService::new(
            metadata.clone(),
            Some(last_fm),
            waiting(wait_for_durations),
        ));

        let running = search.clone();
        let mut result = tokio::spawn(async move { running.get("song").await });
        let background = tokio::time::timeout(Duration::from_secs(5), gate.started.wait_for(Option::is_some))
            .await
            .expect("the durations pass started")
            .expect("open")
            .expect("a flag");

        // The durations pass is still running, so only a search that does not wait can be done.
        assert_eq!(background, !wait_for_durations);
        if wait_for_durations {
            assert!(!result.is_finished());
        } else {
            let songs = tokio::time::timeout(Duration::from_secs(5), &mut result)
                .await
                .expect("in time")
                .expect("joined");
            assert_eq!(songs.len(), 1);
        }
        gate.open.send_replace(true);
        if wait_for_durations {
            let songs = tokio::time::timeout(Duration::from_secs(5), result)
                .await
                .expect("in time")
                .expect("joined");
            assert_eq!(songs.len(), 1);
        }
    }
}

#[tokio::test]
async fn background_durations_run_before_the_video_prewarm() {
    let metadata = one_song_metadata();
    let mut gate = gate_durations(&metadata);
    let (prewarmed_tx, mut prewarmed) = watch::channel(false);
    let prewarmed_tx = Arc::new(prewarmed_tx);
    *metadata.prewarmed.lock() = Some(Arc::new(move || {
        prewarmed_tx.send_replace(true);
    }));
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = ExternalSearchService::new(metadata.clone(), Some(last_fm), waiting(false));

    tokio::time::timeout(Duration::from_secs(5), search.get("song"))
        .await
        .expect("in time");
    tokio::time::timeout(Duration::from_secs(5), gate.started.wait_for(Option::is_some))
        .await
        .expect("the durations pass started")
        .expect("open");
    assert!(!*prewarmed.borrow());

    gate.open.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), prewarmed.wait_for(|done| *done))
        .await
        .expect("prewarmed")
        .expect("open");
}

// ---- Rust-only ---------------------------------------------------------------------------

/// No Last.fm key, or a blank query, is no discovery; a blank album query or limit is none.
#[tokio::test]
async fn nothing_to_search_with_is_no_results() {
    let metadata = one_song_metadata();
    let search = ExternalSearchService::new(metadata.clone(), None, None);
    assert!(search.get("song").await.is_empty());
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = ExternalSearchService::new(metadata, Some(last_fm), None);
    assert!(search.get("   ").await.is_empty());
    assert!(search.get_albums("", 5).await.is_empty());
    assert!(search.get_albums("song", 0).await.is_empty());
}
