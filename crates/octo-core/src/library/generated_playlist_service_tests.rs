//! GeneratedPlaylistSelectTests: genre and decade mixes (#54). The draw is what a listener
//! hears, so its rules are tested directly: it holds still for a period, never breaks the artist
//! cap, and keeps its share for new tracks only when asked to.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use indexmap::IndexMap;

use super::*;
use crate::settings::GenreSettings;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 25, 12, 0, 0).unwrap()
}

/// `created.ToString("O")`: seven fraction digits and a Z.
fn round_trip_text(at: DateTime<Utc>) -> String {
    format!(
        "{}.{:07}Z",
        at.format("%Y-%m-%dT%H:%M:%S"),
        at.timestamp_subsec_nanos() / 100
    )
}

fn song(id: i32, artist: &str, play_count: Option<i32>, created: Option<DateTime<Utc>>) -> Node {
    let created = created.unwrap_or_else(|| now() - TimeDelta::days(730));
    let play_count = play_count.map_or_else(|| "null".to_string(), |count| count.to_string());
    Node::parse(&format!(
        r#"{{"id":"s{id}","title":"Song {id}","artist":"{artist}","artistId":"ar-{artist}","playCount":{play_count},"created":"{}","duration":200}}"#,
        round_trip_text(created)
    ))
    .expect("a song parses")
}

fn pool(count: i32, artists: i32) -> Vec<Node> {
    (0..count)
        .map(|i| song(i, &format!("Artist {}", i % artists), Some(5), None))
        .collect()
}

fn ids(songs: &[Node]) -> String {
    songs
        .iter()
        .map(|song| str_of(song, "id").unwrap_or_default())
        .collect::<Vec<_>>()
        .join(",")
}

fn set_play_count(song: &mut Node, count: i32) {
    song.as_object_mut()
        .expect("an object")
        .insert("playCount".into(), Node::Number(count.to_string()));
}

#[test]
fn select_is_deterministic_for_a_seed() {
    let pool = pool(200, 50);

    let first = select(&pool, 50, 3, 0, 30, now(), 42);
    let again = select(&pool, 50, 3, 0, 30, now(), 42);
    let other = select(&pool, 50, 3, 0, 30, now(), 43);

    assert_eq!(first.len(), 50);
    assert_eq!(ids(&first), ids(&again));
    assert_ne!(ids(&first), ids(&other));
}

#[test]
fn select_never_exceeds_the_artist_cap_even_when_that_leaves_the_mix_short() {
    let pool = pool(100, 4);

    let drawn = select(&pool, 50, 3, 0, 30, now(), 7);

    assert_eq!(drawn.len(), 12);
    let mut per_artist: IndexMap<&str, i32> = IndexMap::new();
    for song in &drawn {
        *per_artist.entry(str_of(song, "artist").unwrap()).or_insert(0) += 1;
    }
    assert!(per_artist.values().all(|count| *count == 3), "{per_artist:?}");
}

#[test]
fn select_new_share_fills_from_new_tracks_first() {
    let mut pool = pool(100, 100);
    for i in 0..10 {
        set_play_count(&mut pool[i * 10], 0);
    }

    let drawn = select(&pool, 20, 3, 50, 30, now(), 1);

    assert_eq!(drawn.len(), 20);
    assert_eq!(drawn.iter().filter(|song| is_new(song, now(), 30)).count(), 10);
}

#[test]
fn select_new_share_zero_ignores_newness() {
    let old = pool(100, 100);
    let mut fresh = pool(100, 100);
    for song in fresh.iter_mut().take(50) {
        set_play_count(song, 0);
    }

    assert_eq!(
        ids(&select(&old, 20, 3, 0, 30, now(), 9)),
        ids(&select(&fresh, 20, 3, 0, 30, now(), 9))
    );
}

/// The draws .NET 9 made from the same pools and seeds: a mix holds the songs the C# build drew.
#[test]
fn select_draws_what_dotnet_drew() {
    assert_eq!(
        ids(&select(&pool(200, 50), 50, 3, 0, 30, now(), 42)),
        "s18,s157,s171,s99,s152,s56,s142,s49,s91,s185,s83,s39,s76,s40,s113,s107,s84,s41,s183,s68,s198,s75,s136,s105,s85,s3,s189,s26,s137,s160,s127,s47,s161,s169,s93,s38,s120,s170,s65,s22,s92,s193,s151,s114,s35,s45,s72,s11,s80,s199"
    );
    assert_eq!(
        ids(&select(&pool(100, 4), 50, 3, 0, 30, now(), 7)),
        "s15,s14,s23,s31,s78,s49,s72,s32,s16,s85,s21,s26"
    );
    let mut fresh = pool(100, 100);
    for i in 0..10 {
        set_play_count(&mut fresh[i * 10], 0);
    }
    assert_eq!(
        ids(&select(&fresh, 20, 3, 50, 30, now(), 1)),
        "s21,s46,s68,s35,s80,s13,s39,s82,s93,s48,s19,s30,s20,s70,s50,s40,s0,s60,s90,s10"
    );
    assert_eq!(
        ids(&select(
            &pool(30, 10),
            10,
            2,
            0,
            30,
            now(),
            seed("alice", "genre:Rock", 1)
        )),
        "s12,s0,s20,s5,s8,s15,s22,s28,s27,s9"
    );
}

#[test]
fn is_new_never_played_or_recently_added() {
    assert!(is_new(&song(1, "A", None, None), now(), 30));
    assert!(is_new(&song(1, "A", Some(0), None), now(), 30));
    assert!(is_new(
        &song(1, "A", Some(4), Some(now() - TimeDelta::days(3))),
        now(),
        30
    ));
    assert!(!is_new(
        &song(1, "A", Some(4), Some(now() - TimeDelta::days(45))),
        now(),
        30
    ));
}

#[test]
fn apply_hysteresis_creates_at_20_keeps_down_to_10_drops_below() {
    let counts: IndexMap<String, i32> = [
        ("genre:New", 20),
        ("genre:Almost", 19),
        ("genre:Kept", 10),
        ("genre:Gone", 9),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    let active = apply_hysteresis(
        &counts,
        &["genre:Kept".to_string(), "genre:Gone".to_string()],
        20,
        10,
        20,
    );

    assert_eq!(active, ["genre:New", "genre:Kept"]);
}

#[test]
fn apply_hysteresis_shows_the_largest_first_up_to_the_limit() {
    let counts: IndexMap<String, i32> = [
        ("genre:B", 50),
        ("genre:A", 50),
        ("decade:1990", 400),
        ("genre:C", 30),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    assert_eq!(
        apply_hysteresis(&counts, &[], 20, 10, 3),
        ["decade:1990", "genre:A", "genre:B"]
    );
}

#[test]
fn parse_genres_one_entry_per_genre_whatever_the_case_and_never_a_year_or_a_blocked_value() {
    let rows = Node::parse(
        r#"[{"value":"Rock","songCount":120},{"value":"rock","songCount":4},{"value":"1990s","songCount":80},
            {"value":"Music","songCount":300},{"value":" ","songCount":10},{"value":"Trip-Hop","songCount":25}]"#,
    )
    .expect("parses");
    let Node::Array(rows) = rows else {
        panic!("an array")
    };

    let counts = parse_genres(Some(&rows), &GenreSettings::default().effective_blocklist());

    let expected: IndexMap<String, i32> = [("genre:Rock", 120), ("genre:Trip-Hop", 25)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    assert_eq!(counts, expected);
}

#[test]
fn ids_are_octo_shaped_and_per_listener() {
    let alice = playlist_id("alice", "genre:Rock");

    assert_eq!(alice.len(), 22);
    assert!(alice.starts_with("og"));
    assert_eq!(alice, playlist_id(" ALICE ", "genre:Rock"));
    assert_ne!(alice, playlist_id("bob", "genre:Rock"));
    assert_ne!(alice, playlist_id("alice", "genre:Pop"));
    // What .NET 9 computed.
    assert_eq!(alice, "ogCBXI0xBkBbo5YFCGFg0J");
    assert_eq!(playlist_id(" ALICE ", "decade:1990"), "oggfZ1V0xbMkjnZt0DRDBI");
}

#[test]
fn period_index_changes_once_per_period() {
    let start = Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap();

    assert_eq!(
        period_index(start, 24),
        period_index(start + TimeDelta::minutes(23 * 60 + 54), 24)
    );
    assert_eq!(
        period_index(start, 24) + 1,
        period_index(start + TimeDelta::hours(24), 24)
    );
    assert_ne!(seed("alice", "genre:Rock", 1), seed("alice", "genre:Rock", 2));
    // What .NET 9 computed.
    assert_eq!(seed("alice", "genre:Rock", 1), -1611485292);
    assert_eq!(seed("alice", "genre:Rock", 2), -847570069);
    assert_eq!(seed("bob", "decade:1990", 20000), -1817182269);
    assert_eq!(dotnet_ticks(now()), 639259344000000000);
}

fn station(i: i32) -> Song {
    Song {
        id: format!("st{i}"),
        artist: format!("Radio Artist {i}"),
        title: format!("Radio Song {i}"),
        ..Default::default()
    }
}

fn insert(node: &mut Node, key: &str, value: Node) {
    node.as_object_mut().expect("an object").insert(key.into(), value);
}

#[test]
fn blend_puts_new_library_songs_from_the_end_and_never_repeats_one() {
    let songs: Vec<Song> = (0..10).map(station).collect();
    let mut candidates = vec![
        song(100, "Radio Artist 3", Some(0), None),
        song(101, "Library One", Some(0), None),
        song(102, "Library Two", Some(7), None),
        song(103, "Library Three", Some(0), None),
    ];
    insert(&mut candidates[0], "title", Node::String("Radio Song 3".into()));
    insert(&mut candidates[1], "suffix", Node::String("mp3".into()));
    insert(&mut candidates[1], "bitRate", Node::Number("320".into()));
    insert(
        &mut candidates[1],
        "isrc",
        Node::Array(vec![
            Node::String("USRC17607839".into()),
            Node::String("us-rc1-76-07840".into()),
        ]),
    );

    let blended = blend(&songs, &candidates, 2, 30, now()).expect("two were put in");

    assert_eq!(blended.len(), 10);
    assert_eq!(blended[9].id, "s101");
    assert_eq!(blended[4].id, "s103");
    assert!(blended[9].is_local);
    assert_eq!(blended[9].suffix.as_deref(), Some("mp3"));
    assert_eq!(blended[9].bit_rate, Some(320));
    // The library's own ISRCs go back out exactly as Navidrome sent them.
    assert_eq!(
        blended[9].isrcs_for_clients(),
        ["USRC17607839", "us-rc1-76-07840"]
    );
    assert!(blended[4].isrcs_for_clients().is_empty());
    assert_eq!(blended.iter().filter(|song| song.id.starts_with("st")).count(), 8);
}

#[test]
fn blend_nothing_new_leaves_the_station_alone() {
    let songs: Vec<Song> = (0..10).map(station).collect();

    assert!(blend(&songs, &[song(1, "Library", Some(3), None)], 2, 30, now()).is_none());
}

/// state-files.md §4.14: the file reads and writes back byte for byte, compact.
#[test]
fn the_state_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/generated-playlists.json"
    ))
    .expect("the fixture is in the repo");
    let document: StateDocument = serde_json::from_str(&text).expect("the fixture reads");
    let users = document.users.as_ref().expect("users");
    assert_eq!(users["brandon"].counts["genre:R&B"], 12);
    assert_eq!(users["brandon"].kinds, "genre,decade");
    assert_eq!(crate::json::to_string(&document), text);
}

#[test]
fn kinds_and_staleness_follow_the_settings() {
    let settings = GeneratedPlaylistSettings {
        genres: true,
        decades: false,
        ..Default::default()
    };
    assert_eq!(kinds_of(&settings), "genre,");
    let state = UserMixes {
        counts_utc: now(),
        kinds: "genre,".into(),
        ..Default::default()
    };
    assert!(!is_stale(&state, &settings, now()));
    assert!(is_stale(
        &state,
        &settings,
        now() + TimeDelta::hours(i64::from(settings.effective_refresh_hours()))
    ));
    assert!(is_stale(&UserMixes::default(), &settings, now()));
    assert_eq!(
        describe("decade:1990"),
        ("decade".to_string(), "1990s".to_string())
    );
    assert_eq!(describe("genre:Rock"), ("genre".to_string(), "Rock".to_string()));
}

#[test]
fn numbers_read_as_json_node_reads_them() {
    let node = Node::parse(r#"{"a":5,"b":5.9,"c":1e3,"d":"5","e":null,"f":99999999999}"#).unwrap();
    assert_eq!(nullable_int_of(&node, "a"), Some(5));
    assert_eq!(nullable_int_of(&node, "b"), Some(5));
    assert_eq!(nullable_int_of(&node, "c"), Some(1000));
    assert_eq!(nullable_int_of(&node, "d"), None);
    assert_eq!(nullable_int_of(&node, "e"), None);
    assert_eq!(nullable_int_of(&node, "f"), Some(i32::MAX));
    assert_eq!(int_of(&node, "missing"), 0);
}
