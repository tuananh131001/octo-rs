//! GeneratedPlaylistServiceTests: the service end to end against a stand-in Navidrome:
//! counting, listing, the draw a playlist read returns, and the Discovery blend.

use std::collections::HashMap;
use std::sync::Arc;

use octo_core::settings::{AppSettings, SubsonicSettings};
use parking_lot::Mutex;
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;

/// The C# `LibraryNavidrome`.
#[derive(Clone, Default)]
struct LibraryNavidrome {
    calls: Arc<Mutex<Vec<String>>>,
}

impl LibraryNavidrome {
    fn genre_calls(&self) -> usize {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.starts_with("rest/getGenres"))
            .count()
    }
}

fn ok(inner: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{}}}}}"#,
        if inner.is_empty() {
            String::new()
        } else {
            format!(",{inner}")
        }
    )
}

fn songs(count: usize, first: usize, play_count: i32) -> String {
    (first..first + count)
        .map(|i| {
            format!(
                r#"{{"id":"lib{i}","title":"Track {i}","artist":"Artist {}","artistId":"ar{}","duration":200,"playCount":{play_count},"suffix":"flac","bitRate":900}}"#,
                i % 20,
                i % 20
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().trim_matches('/').to_string();
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        self.calls
            .lock()
            .push(format!("{path}?{}", request.url.query().unwrap_or_default()));
        let body = match path.as_str() {
            "rest/getGenres" => {
                ok(r#""genres":{"genre":[{"value":"Rock","songCount":40},{"value":"Polka","songCount":3}]}"#)
            }
            "rest/getSongsByGenre" => {
                let first = if query.get("offset").map(String::as_str) == Some("0") {
                    0
                } else {
                    1000
                };
                ok(&format!(r#""songsByGenre":{{"song":[{}]}}"#, songs(40, first, 5)))
            }
            "rest/getRandomSongs" => match query.get("fromYear").map(String::as_str) {
                Some("1990") => ok(&format!(r#""randomSongs":{{"song":[{}]}}"#, songs(25, 2000, 5))),
                None => ok(&format!(r#""randomSongs":{{"song":[{}]}}"#, songs(30, 3000, 0))),
                Some(_) => ok(r#""randomSongs":{"song":[]}"#),
            },
            _ => ok(""),
        };
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "application/json")
            .set_body_string(body)
    }
}

struct Fixture {
    dir: TempDir,
    server: MockServer,
    navidrome: LibraryNavidrome,
}

async fn fixture() -> Fixture {
    let navidrome = LibraryNavidrome::default();
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome.clone())
        .mount(&server)
        .await;
    Fixture {
        dir: TempDir::new().unwrap(),
        server,
        navidrome,
    }
}

fn service(fixture: &Fixture, settings: GeneratedPlaylistSettings) -> Arc<GeneratedPlaylistService> {
    let store = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(fixture.server.uri()),
            ..Default::default()
        },
        generated_playlists: settings,
        ..Default::default()
    }));
    Arc::new(GeneratedPlaylistService::new(
        Some(fixture.dir.path().join("generated-playlists.json")),
        SubsonicProxyService::new(store.clone()),
        store,
    ))
}

fn auth() -> IndexMap<String, String> {
    [
        ("u", "alice"),
        ("t", "token"),
        ("s", "salt"),
        ("v", "1.16.1"),
        ("c", "test"),
        ("id", "not-for-navidrome"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn enabled() -> GeneratedPlaylistSettings {
    GeneratedPlaylistSettings {
        enabled: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn list_counts_the_library_and_offers_what_clears_the_threshold() {
    let fixture = fixture().await;
    let service = service(&fixture, enabled());

    let mixes = service.list("alice", &auth()).await;

    let names: Vec<&str> = mixes.iter().map(|mix| mix.name.as_str()).collect();
    assert_eq!(names, ["Rock Mix", "1990s Mix"]);
    assert!(mixes.iter().all(|mix| mix.owner == "alice"));
    assert!(
        !fixture
            .navidrome
            .calls
            .lock()
            .iter()
            .any(|call| call.contains("id=not-for-navidrome"))
    );
    assert_eq!(service.find("alice", &mixes[0].id), Some(mixes[0].clone()));
    assert!(service.find("bob", &mixes[0].id).is_none());
}

#[tokio::test]
async fn list_while_fresh_does_not_count_again_and_survives_a_restart() {
    let fixture = fixture().await;
    service(&fixture, enabled()).list("alice", &auth()).await;
    service(&fixture, enabled()).list("alice", &auth()).await;

    let restarted = service(&fixture, enabled());
    let mixes = restarted.list("alice", &auth()).await;

    assert_eq!(fixture.navidrome.genre_calls(), 1);
    assert_eq!(mixes.len(), 2);
}

#[tokio::test]
async fn list_off_is_nothing() {
    let fixture = fixture().await;
    assert!(
        service(&fixture, GeneratedPlaylistSettings::default())
            .list("alice", &auth())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn materialize_draws_from_the_genre_and_holds_still_for_the_period() {
    let fixture = fixture().await;
    let service = service(
        &fixture,
        GeneratedPlaylistSettings {
            enabled: true,
            track_count: 10,
            max_per_artist: 1,
            ..Default::default()
        },
    );
    let rock = service
        .list("alice", &auth())
        .await
        .into_iter()
        .find(|mix| mix.kind == "genre")
        .expect("the rock mix");

    let none = CancellationToken::new();
    let first = service.materialize("alice", &rock, &auth(), &none).await.unwrap();
    let second = service.materialize("alice", &rock, &auth(), &none).await.unwrap();

    assert_eq!(first.len(), 10);
    let mut artists: Vec<&str> = first
        .iter()
        .map(|song| song.get("artist").and_then(Node::as_str).unwrap())
        .collect();
    artists.sort();
    artists.dedup();
    assert_eq!(artists.len(), 10);
    let text = |songs: &[Node]| {
        songs
            .iter()
            .map(|song| song.to_json_string(false))
            .collect::<Vec<_>>()
    };
    assert_eq!(text(&first), text(&second));
    assert_eq!(
        fixture
            .navidrome
            .calls
            .lock()
            .iter()
            .filter(|call| call.starts_with("rest/getSongsByGenre"))
            .count(),
        1
    );
    assert_eq!(service.drawn("alice", &rock).map(|songs| songs.len()), Some(10));
}

fn ids(songs: &[Song]) -> Vec<String> {
    songs.iter().map(|song| song.id.clone()).collect()
}

#[tokio::test]
async fn blend_only_discovery_and_only_with_a_share() {
    let fixture = fixture().await;
    let songs: Vec<Song> = (0..10)
        .map(|i| Song {
            id: format!("st{i}"),
            artist: format!("A{i}"),
            title: format!("T{i}"),
            ..Default::default()
        })
        .collect();
    let discovery = LastFmRadioStation {
        id: "orStation".into(),
        kind: LastFmRadioStationKind::Discovery,
        changed_utc: Utc::now(),
        valid_until_utc: Utc::now() + TimeDelta::hours(6),
        ..Default::default()
    };
    let your_mix = LastFmRadioStation {
        id: "orMix".into(),
        kind: LastFmRadioStationKind::YourMix,
        changed_utc: Utc::now(),
        ..Default::default()
    };

    let none = service(&fixture, GeneratedPlaylistSettings::default());
    assert_eq!(
        ids(&none
            .blend_into_discovery("alice", &discovery, songs.clone(), &auth())
            .await),
        ids(&songs)
    );

    let shared = service(
        &fixture,
        GeneratedPlaylistSettings {
            new_share: 20,
            ..Default::default()
        },
    );
    assert_eq!(
        ids(&shared
            .blend_into_discovery("alice", &your_mix, songs.clone(), &auth())
            .await),
        ids(&songs)
    );
    let blended = shared
        .blend_into_discovery("alice", &discovery, songs.clone(), &auth())
        .await;
    assert_eq!(blended.iter().filter(|song| song.is_local).count(), 2);
}

/// Rust-only: the file written is the compact state document, a bad file is set aside as
/// `.corrupt-<ticks>`, and the query keeps the caller's credentials in the C# order.
#[tokio::test]
async fn the_state_file_is_written_and_a_bad_one_is_set_aside() {
    let fixture = fixture().await;
    let path = fixture.dir.path().join("generated-playlists.json");
    service(&fixture, enabled()).list("Alice", &auth()).await;
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.starts_with(r#"{"Users":{"alice":{"Active":["genre:Rock","decade:1990"],"Counts":{"genre:Rock":40,"genre:Polka":3,"decade:1950":0"#), "{written}");

    let first = fixture.navidrome.calls.lock()[0].clone();
    assert_eq!(
        first,
        "rest/getGenres?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json"
    );

    std::fs::write(&path, "not json").unwrap();
    let reloaded = service(&fixture, enabled());
    assert!(reloaded.find("alice", "og").is_none());
    let names: Vec<String> = std::fs::read_dir(fixture.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|name| name.starts_with("generated-playlists.json.corrupt-")),
        "{names:?}"
    );
}

/// GeneratedPlaylistResponseTests: what a client is told about a mix and its songs, and that a
/// library MP3 is an MP3.
mod response {
    use chrono::TimeZone;
    use octo_core::settings::SubsonicSettings;
    use octo_subsonic::subsonic_response_builder::{ReplyKind, SubsonicResponseBuilder};
    use serde_json::Value;

    use super::*;
    use crate::services::soulseek::ExternalIdRegistry;
    use crate::services::subsonic::subsonic_response_builder::{
        SubsonicResponseBuilderExt, new_subsonic_response_builder,
    };

    fn builder() -> SubsonicResponseBuilder {
        new_subsonic_response_builder(
            Arc::new(ExternalIdRegistry::in_memory()),
            &SubsonicSettings::default(),
        )
    }

    fn mix() -> GeneratedPlaylist {
        GeneratedPlaylist {
            id: format!("og{}", "a".repeat(20)),
            key: "genre:Rock".into(),
            kind: "genre".into(),
            label: "Rock".into(),
            name: "Rock Mix".into(),
            owner: "alice".into(),
            pool_size: 40,
            period_start_utc: Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap(),
            period_end_utc: Utc.with_ymd_and_hms(2026, 9, 26, 0, 0, 0).unwrap(),
        }
    }

    fn entry() -> Node {
        Node::parse(
            r#"{"id":"lib1","title":"Track","artist":"Artist","duration":200,"playCount":3,"genres":[{"name":"Rock"}],"replayGain":{"trackGain":-6.1}}"#,
        )
        .unwrap()
    }

    #[test]
    fn row_is_read_only_and_owned_by_the_listener() {
        let fields = builder().generated_playlist_fields(
            &mix(),
            &GeneratedPlaylistSettings {
                track_count: 100,
                ..Default::default()
            },
        );

        assert_eq!(fields["name"], "Rock Mix");
        assert_eq!(fields["owner"], "alice");
        assert_eq!(fields["readonly"], true);
        assert_eq!(fields["songCount"], 40);
        assert_eq!(fields["coverArt"], Value::String(mix().id));
    }

    #[test]
    fn playlist_json_passes_navidromes_songs_through() {
        let reply = builder().create_generated_playlist_response(
            "json",
            &mix(),
            &GeneratedPlaylistSettings::default(),
            &[entry()],
        );
        assert_eq!(reply.kind, ReplyKind::Json);
        let document: Value = serde_json::from_str(&reply.text()).unwrap();
        let playlist = &document["subsonic-response"]["playlist"];

        assert_eq!(playlist["songCount"], 1);
        assert_eq!(playlist["duration"], 200);
        let song = &playlist["entry"][0];
        assert_eq!(song["id"], "lib1");
        assert_eq!(song["replayGain"]["trackGain"], -6.1);
    }

    #[test]
    fn playlist_xml_carries_every_scalar_as_an_attribute() {
        let reply = builder().create_generated_playlist_response(
            "xml",
            &mix(),
            &GeneratedPlaylistSettings::default(),
            &[entry()],
        );
        assert_eq!(reply.kind, ReplyKind::Content);
        let text = reply.text();
        let playlist = text.lines().nth(1).unwrap();
        let entry = text.lines().nth(2).unwrap();

        assert!(playlist.contains(r#"readonly="true""#), "{playlist}");
        assert!(entry.contains(r#"id="lib1""#), "{entry}");
        assert!(entry.contains(r#"duration="200""#), "{entry}");
        assert!(!entry.contains("genres="), "{entry}");
    }

    /// A strict client prepares its decoder from these; an MP3 declared as FLAC fails.
    #[test]
    fn convert_song_local_mp3_is_declared_as_mp3() {
        let mp3 = builder().convert_song_to_json(&Song {
            id: "l1".into(),
            artist: "A".into(),
            title: "T".into(),
            is_local: true,
            suffix: Some("MP3".into()),
            bit_rate: Some(320),
            duration: Some(100),
            ..Default::default()
        });
        let unknown = builder().convert_song_to_json(&Song {
            id: "l2".into(),
            artist: "A".into(),
            title: "T".into(),
            is_local: true,
            duration: Some(100),
            ..Default::default()
        });

        assert_eq!(mp3["suffix"], "mp3");
        assert_eq!(mp3["contentType"], "audio/mpeg");
        assert_eq!(mp3["bitRate"], 320);
        assert_eq!(unknown["suffix"], "flac");
        assert_eq!(unknown["bitRate"], 1411);
    }
}
