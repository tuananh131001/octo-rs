//! The pure halves of the SoulseekClient tests: SoulseekTransferPollTests, the parsing half of
//! SoulseekOutageTests, the payload half of SoulseekSearchProfileTests, SoulseekSlowTransferTests'
//! TransferWatch tests, ParallelDownloadTests' batch shapes, AlbumFolderTests' folder listings,
//! AcquisitionTrackerTests' progress reads and SoulseekCandidateMatchingTests' extension tests.

use chrono::TimeZone;
use serde_json::Value;

use super::*;
use crate::settings::SoulseekSettings;

fn parse(json: &str) -> Value {
    serde_json::from_str(json).expect("valid JSON")
}

// ---- SoulseekTransferPollTests --------------------------------------------------------------

const REMOTE_FILENAME: &str = r"music\Daft Punk\1997 - Homework [CD]\01 - Daftendirekt.flac";

// Shape of GET /api/v0/transfers/downloads/{username}: one user group object.
const PER_USER_OBJECT_JSON: &str = r#"
{
  "username": "blixquoy",
  "directories": [
    {
      "directory": "music\\Daft Punk\\1997 - Homework [CD]",
      "fileCount": 1,
      "files": [
        {
          "filename": "music\\Daft Punk\\1997 - Homework [CD]\\01 - Daftendirekt.flac",
          "state": "Completed, Succeeded",
          "size": 17361963
        }
      ]
    }
  ]
}
"#;

fn array_wrapped() -> String {
    format!("[{PER_USER_OBJECT_JSON}]")
}

fn find(json: &str, filename: &str) -> Option<String> {
    find_transfer_state(&parse(json), filename).expect("a well-formed answer")
}

#[test]
fn per_user_object_response_finds_completed_transfer() {
    assert_eq!(
        find(PER_USER_OBJECT_JSON, REMOTE_FILENAME).as_deref(),
        Some("Completed, Succeeded")
    );
}

#[test]
fn all_users_array_response_finds_completed_transfer() {
    assert_eq!(
        find(&array_wrapped(), REMOTE_FILENAME).as_deref(),
        Some("Completed, Succeeded")
    );
}

#[test]
fn file_not_in_response_returns_null() {
    assert_eq!(find(PER_USER_OBJECT_JSON, r"music\Other\file.flac"), None);
    assert_eq!(find(&array_wrapped(), r"music\Other\file.flac"), None);
}

#[test]
fn errored_state_is_returned_verbatim() {
    let json = PER_USER_OBJECT_JSON.replace("Completed, Succeeded", "Completed, Errored");
    assert_eq!(
        find(&json, REMOTE_FILENAME).as_deref(),
        Some("Completed, Errored")
    );
}

#[test]
fn malformed_roots_return_null() {
    assert_eq!(find("\"just a string\"", REMOTE_FILENAME), None);
    assert_eq!(find("{\"username\":\"x\"}", REMOTE_FILENAME), None);
    assert_eq!(
        find("[{\"directories\":\"not-an-array\"}]", REMOTE_FILENAME),
        None
    );
}

/// Not in the C#: a directory that is not an object made `TryGetProperty` throw, which the
/// poll logged as transient. The reader still says so.
#[test]
fn a_directory_that_is_not_an_object_throws_as_json_element_did() {
    assert!(find_transfer(&parse(r#"{"directories":[5]}"#), "f", None).is_err());
}

// ---- SoulseekOutageTests (the parsing half) -----------------------------------------------

const LOGGED_IN_JSON: &str = r#"
{"version":{"current":"0.26.0"},
 "server":{"address":"server.slsknet.org","ipEndPoint":"208.76.170.59:2271","state":"Connected, LoggedIn",
           "isConnected":true,"isConnecting":false,"isLoggedIn":true,"isLoggingIn":false,"isTransitioning":false},
 "user":{"username":"winters27"},
 "connectionWatchdog":{"isEnabled":true,"isAttemptingConnection":false}}
"#;

// Disconnected: slskd leaves address and ipEndPoint out entirely.
const DISCONNECTING_JSON: &str = r#"
{"server":{"state":"Disconnecting","isConnected":false,"isLoggedIn":false,"isTransitioning":true},
 "user":{"username":"winters27"},
 "connectionWatchdog":{"isEnabled":true,"isAttemptingConnection":true,"nextAttemptAt":"2026-10-03T05:48:13Z"}}
"#;

#[test]
fn a_logged_in_slskd_reads_as_logged_in() {
    let reading = parse_server_reading(LOGGED_IN_JSON);
    assert_eq!(reading.link, SoulseekLinkState::LoggedIn);
    assert_eq!(reading.state.as_deref(), Some("Connected, LoggedIn"));
    assert_eq!(reading.username.as_deref(), Some("winters27"));
}

#[test]
fn a_disconnecting_slskd_reads_as_not_logged_in_with_its_next_try() {
    let reading = parse_server_reading(DISCONNECTING_JSON);
    assert_eq!(reading.link, SoulseekLinkState::NotLoggedIn);
    assert_eq!(reading.state.as_deref(), Some("Disconnecting"));
    assert_eq!(
        reading.next_attempt_utc,
        Some(
            Utc.with_ymd_and_hms(2026, 10, 3, 5, 48, 13)
                .single()
                .expect("a date")
        )
    );
}

#[test]
fn a_shape_without_the_flags_is_unknown() {
    for json in ["{}", r#"{"server":{"state":"Connected"}}"#, "not json", "[]"] {
        assert_eq!(
            parse_server_reading(json).link,
            SoulseekLinkState::Unknown,
            "{json}"
        );
    }
}

/// The parsed halves of TheDashboardWarnsWhenSlskdIsUpButNotLoggedIn and
/// TheDashboardLinesForTheOtherStates (the rest is in `soulseek_link`).
#[test]
fn the_dashboard_reads_slskds_own_answers() {
    use crate::soulseek::soulseek_link::describe;
    let (ok, warning, detail) = describe(Some(&parse_server_reading(DISCONNECTING_JSON)), 6);
    assert!(ok && warning);
    assert!(detail.contains("slskd says Disconnecting"), "{detail}");
    assert!(detail.contains("05:48 UTC"), "{detail}");
    assert_eq!(
        describe(Some(&parse_server_reading(LOGGED_IN_JSON)), 6),
        (true, false, "logged in to Soulseek as winters27".to_string())
    );
}

#[test]
fn the_flags_and_names_are_read_ignoring_case() {
    let reading =
        parse_server_reading(r#"{"Server":{"IsConnected":true,"ISLOGGEDIN":true},"USER":{"UserName":"x"}}"#);
    assert_eq!(reading.link, SoulseekLinkState::LoggedIn);
    assert_eq!(reading.username.as_deref(), Some("x"));
}

// ---- SoulseekSearchProfileTests.ThePayloadCarriesEachProfilesLimits (the payload half) ----

#[test]
fn the_payload_carries_each_profiles_limits() {
    let limits = |p: &SearchProfile| {
        let root = parse(&search_payload(
            "00000000-0000-0000-0000-000000000000",
            "Artist Song",
            p,
        ));
        (
            root["searchTimeout"].as_i64(),
            root["responseLimit"].as_i64(),
            root["fileLimit"].as_i64(),
            root["filterResponses"].as_bool(),
        )
    };
    let s = SoulseekSettings::default();
    assert_eq!(
        limits(&SearchProfile::interactive(&s)),
        (Some(15_000), Some(250), Some(500), Some(true))
    );
    assert_eq!(
        limits(&SearchProfile::upgrade(&s)),
        (Some(30_000), Some(500), Some(2_000), Some(true))
    );
    assert_eq!(
        search_payload("id", "Artist Song", &SearchProfile::interactive(&s)),
        r#"{"id":"id","searchText":"Artist Song","searchTimeout":15000,"responseLimit":250,"fileLimit":500,"filterResponses":true}"#
    );
}

// ---- SoulseekSlowTransferTests (TransferWatch) -------------------------------------------

fn at(t0: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
    t0 + TimeDelta::seconds(seconds)
}

#[test]
fn a_watch_keeps_waiting_while_bytes_arrive() {
    let t0 = Utc::now();
    let mut watch = TransferWatch::new(t0, Duration::from_secs(10), Duration::from_secs(3600));
    for s in (5..=300).step_by(5) {
        watch.saw(Some(s * 1000), at(t0, s));
    }
    assert!(!watch.expired(at(t0, 305)));
    assert!(watch.expired(at(t0, 310)));
}

#[test]
fn a_watch_gives_up_when_nothing_new_arrives() {
    let t0 = Utc::now();
    let mut watch = TransferWatch::new(t0, Duration::from_secs(10), Duration::from_secs(3600));
    watch.saw(Some(0), at(t0, 3));
    watch.saw(Some(500), at(t0, 4));
    // The same count again is not progress.
    watch.saw(Some(500), at(t0, 9));
    watch.saw(None, at(t0, 12));
    assert!(!watch.expired(at(t0, 13)));
    assert!(watch.expired(at(t0, 14)));
}

#[test]
fn a_watch_stops_at_the_ceiling_even_while_moving() {
    let t0 = Utc::now();
    let mut watch = TransferWatch::new(t0, Duration::from_secs(10), Duration::from_secs(60));
    watch.saw(Some(1), at(t0, 59));
    assert!(!watch.expired(at(t0, 59)));
    assert!(watch.expired(at(t0, 60)));
    assert!(watch.hit_ceiling(at(t0, 60)));
}

// ---- ParallelDownloadTests (slskd's batch API) -------------------------------------------

const JOB: &str = ".octo-incoming/slskd/job";

#[test]
fn the_batch_payload_has_slskds_shape() {
    let root = parse(&batch_payload("peer", &[("a\\b.flac".to_string(), 123)], JOB));
    assert!(uuid::Uuid::parse_str(root["id"].as_str().expect("an id")).is_ok());
    assert_eq!(root["username"], "peer");
    let files = root["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["filename"], "a\\b.flac");
    assert_eq!(files[0]["size"].as_i64(), Some(123));
    assert_eq!(root["options"]["destination"], JOB);
}

#[test]
fn a_batch_answer_gives_each_files_transfer_id_and_the_failures() {
    let batch = parse_batch(
        r#"
        {"batch":{"id":"b","username":"peer","transfers":[
            {"id":"t-1","filename":"a\\1.flac","state":"Queued, Locally"},
            {"id":"t-2","filename":"a\\2.flac","state":"Queued, Locally"}]},
         "failures":[{"filename":"a\\3.flac","message":"File not shared."}]}
        "#,
    )
    .expect("a readable answer");
    assert!(batch.supported);
    assert_eq!(batch.transfer_ids["a\\1.flac"], "t-1");
    assert_eq!(batch.transfer_ids["a\\2.flac"], "t-2");
    assert_eq!(
        batch.failures,
        vec![("a\\3.flac".to_string(), "File not shared.".to_string())]
    );
}

#[test]
fn an_unreadable_batch_answer_is_nothing_queued() {
    let batch = parse_batch("not json").expect("bad JSON is caught");
    assert!(batch.supported && batch.transfer_ids.is_empty() && batch.failures.is_empty());
    assert!(parse_batch(r#"{"batch":{"transfers":[7]}}"#).is_err());
}

#[test]
fn a_transfer_is_followed_by_its_id_not_an_older_one_of_the_same_name() {
    let root = parse(
        r#"
        {"username":"peer","directories":[{"directory":"a","files":[
            {"id":"old","filename":"a\\f.flac","state":"Completed, Cancelled"},
            {"id":"new","filename":"a\\f.flac","state":"InProgress"}]}]}
        "#,
    );
    assert_eq!(
        find_transfer_state(&root, "a\\f.flac").unwrap().as_deref(),
        Some("Completed, Cancelled")
    );
    let newer = find_transfer(&root, "a\\f.flac", Some("new"))
        .unwrap()
        .expect("found by id");
    assert_eq!(transfer_id(newer).unwrap().as_deref(), Some("new"));
    assert!(find_transfer(&root, "a\\f.flac", Some("gone")).unwrap().is_none());
}

// ---- AlbumFolderTests (folder listings) ----------------------------------------------------

#[test]
fn a_folders_files_come_back_with_their_full_remote_path() {
    let from = SoulseekFileHit {
        username: "peer".into(),
        filename: r"A\B\x.mp3".into(),
        queue_length: Some(2),
        upload_speed: Some(900),
        has_free_upload_slot: Some(true),
        ..Default::default()
    };
    let hits = parse_directory(
        r#"[{"name":"A\\B","files":[{"filename":"01 - Song.flac","size":30000000,"extension":"flac","length":200,"bitDepth":16,"sampleRate":44100}]}]"#,
        &from,
        r"A\B",
    )
    .expect("readable");
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit.filename, r"A\B\01 - Song.flac");
    assert_eq!(hit.extension, "flac");
    assert_eq!(hit.length, Some(200));
    assert_eq!(hit.queue_length, Some(2));
    assert_eq!(hit.has_free_upload_slot, Some(true));
}

#[test]
fn a_folder_object_and_full_paths_are_read_too() {
    let from = SoulseekFileHit {
        username: "peer".into(),
        filename: r"A\B\x.mp3".into(),
        ..Default::default()
    };
    let hits = parse_directory(
        r#"{"files":[{"filename":"A\\B\\02 - Other.flac","size":1}]}"#,
        &from,
        r"A\B",
    )
    .expect("readable");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].filename, r"A\B\02 - Other.flac");
    assert_eq!(hits[0].extension, "flac");
}

#[test]
fn a_listing_that_is_not_json_or_of_the_wrong_kind_fails_its_own_way() {
    let from = SoulseekFileHit::default();
    assert!(matches!(
        parse_directory("nope", &from, "A"),
        Err(DirectoryError::Json(_))
    ));
    assert!(matches!(
        parse_directory(r#"[{"files":[{"filename":5}]}]"#, &from, "A"),
        Err(DirectoryError::Element(_))
    ));
}

// ---- AcquisitionTrackerTests (the SoulseekClient halves) ---------------------------------

// The per-user transfer shape slskd 0.26 returns, with a transfer part way through.
const IN_PROGRESS_TRANSFER: &str = r#"
{
  "username": "blixquoy",
  "directories": [
    {
      "directory": "music\\Daft Punk\\1997 - Homework [CD]",
      "files": [
        {
          "filename": "music\\Daft Punk\\1997 - Homework [CD]\\01 - Daftendirekt.flac",
          "state": "InProgress",
          "size": 29000000,
          "bytesTransferred": 12180000,
          "percentComplete": 42.0
        }
      ]
    }
  ]
}
"#;

#[test]
fn a_transfer_poll_becomes_the_rows_progress() {
    let root = parse(IN_PROGRESS_TRANSFER);
    let file = find_transfer(&root, REMOTE_FILENAME, None)
        .unwrap()
        .expect("found");
    let progress = read_transfer_progress(file).unwrap();
    assert_eq!(progress.bytes_transferred, Some(12_180_000));
    assert_eq!(progress.size, Some(29_000_000));
    assert_eq!(progress.percent_complete, Some(42.0));
    assert!(progress.is_moving());
}

#[test]
fn a_transfer_waiting_in_the_peers_queue_is_not_moving() {
    let json = IN_PROGRESS_TRANSFER
        .replace("\"InProgress\"", "\"Queued, Remotely\"")
        .replace("12180000", "0")
        .replace("42.0", "0");
    let root = parse(&json);
    let progress = read_transfer_progress(
        find_transfer(&root, REMOTE_FILENAME, None)
            .unwrap()
            .expect("found"),
    )
    .unwrap();
    assert!(!progress.is_moving());
    assert_eq!(
        find_transfer_state(&root, REMOTE_FILENAME).unwrap().as_deref(),
        Some("Queued, Remotely")
    );
}

#[test]
fn missing_figures_read_as_unknown_not_zero() {
    let root = parse(
        r#"{"username":"u","directories":[{"files":[{"filename":"f.flac","state":"InProgress","size":"big"}]}]}"#,
    );
    let progress =
        read_transfer_progress(find_transfer(&root, "f.flac", None).unwrap().expect("found")).unwrap();
    assert_eq!(progress.bytes_transferred, None);
    assert_eq!(progress.size, None);
    assert_eq!(progress.percent_complete, None);
}

// ---- SoulseekCandidateMatchingTests (extension normalization) ----------------------------

/// Ranking accepts a hit by comparing its extension against the configured one, and
/// slskd does not report a consistent shape. An unnormalized ".flac" matched no
/// configured "flac", which surfaced as "this track is not on Soulseek" rather than
/// as a parsing mismatch, and took every FLAC on the network with it.
#[test]
fn every_shape_slskd_reports_reduces_to_the_same_extension() {
    assert_eq!(normalize_extension(Some(".flac"), "x.flac"), "flac");
    assert_eq!(normalize_extension(Some("flac"), "x.flac"), "flac");
    assert_eq!(normalize_extension(Some("FLAC"), "x.flac"), "flac");
    assert_eq!(normalize_extension(Some("  .FLAC "), "x.flac"), "flac");
}

/// Field absent or blank: fall back to the filename it came with.
#[test]
fn a_missing_extension_falls_back_to_the_filename() {
    assert_eq!(normalize_extension(None, r"share\Artist\01 - Track.flac"), "flac");
    assert_eq!(
        normalize_extension(Some(""), r"share\Artist\01 - Track.flac"),
        "flac"
    );
    assert_eq!(normalize_extension(None, "no extension at all"), "");
}

// ---- The rest of the readers (not in the C# tests) -----------------------------------------

#[test]
fn search_responses_carry_each_peers_queue_and_speed() {
    let (hits, error) = parse_responses(
        r#"[{"username":"peer","uploadSpeed":1000000,"queueLength":0,"hasFreeUploadSlot":true,"files":[
              {"filename":"Music\\Artist\\01 - Song.flac","size":30000000,"extension":"flac","length":200},
              {"filename":"  ","size":1}]},
            {"username":" ","files":[{"filename":"x.flac"}]},
            {"username":"other","files":"none"}]"#,
    );
    assert_eq!(error, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].upload_speed, Some(1_000_000));
    assert_eq!(hits[0].has_free_upload_slot, Some(true));
    assert_eq!(hits[0].extension, "flac");
}

#[test]
fn a_bad_response_ends_the_read_but_keeps_what_came_before() {
    let (hits, error) = parse_responses(
        r#"[{"username":"a","files":[{"filename":"1.flac"}]},{"username":"b","uploadSpeed":1.5,"files":[]}]"#,
    );
    assert_eq!(hits.len(), 1);
    assert!(error.is_some());
    let (none, error) = parse_responses("{}");
    assert!(none.is_empty() && error.is_none());
    assert!(parse_responses("nope").1.is_some());
}

#[test]
fn a_search_record_is_ended_only_once_ended_at_is_set() {
    let status = parse_search_status(r#"{"state":"Completed, TimedOut","responseCount":3,"endedAt":null}"#)
        .unwrap()
        .expect("a record");
    assert!(!status.ended);
    assert_eq!(status.response_count, 3);
    let ended = parse_search_status(r#"{"state":"Completed","endedAt":"2026-10-02T00:00:01Z"}"#)
        .unwrap()
        .expect("a record");
    assert!(ended.ended);
    assert_eq!(parse_search_status("[]").unwrap(), None);
    assert!(parse_search_status("nope").is_err());
}

#[test]
fn the_directory_options_are_read_ignoring_case() {
    assert_eq!(
        parse_directory_option(r#"{"Directories":{"Downloads":"/music"}}"#, "downloads").unwrap(),
        Some("/music".to_string())
    );
    assert_eq!(parse_directory_option("[]", "downloads").unwrap(), None);
    assert_eq!(
        parse_directory_option(r#"{"directories":{"incomplete":5}}"#, "incomplete").unwrap(),
        None
    );
}

#[test]
fn the_request_bodies_are_written_as_system_text_json_wrote_them() {
    assert_eq!(
        enqueue_payload("a\\b.flac", 5),
        r#"[{"filename":"a\\b.flac","size":5}]"#
    );
    assert_eq!(browse_payload("A\\B"), r#"{"directory":"A\\B"}"#);
    assert_eq!(
        session_payload("u", "p+w"),
        // The default encoder escapes "+" (one of its HTML-sensitive characters).
        "{\"username\":\"u\",\"password\":\"p\\u002Bw\"}"
    );
    let (token, expires) = parse_session(r#"{"token":"jwt","expires":4102444800}"#).unwrap();
    assert_eq!(token.as_deref(), Some("jwt"));
    assert_eq!(expires.timestamp(), 4_102_444_800);
    assert!(parse_session(r#"{"expires":1}"#).is_err());
}
