//! `DuplicateTests.cs`: what counts as a copy, which copy is suggested, and that a question a
//! person settled stays settled (#53). `DuplicateSettingsTests`' two settings tests went with
//! 1-A; its `ScanNow_WithDuplicatesOff_SaysWhatToTurnOn` is the admin controller's (6-B).

use std::collections::HashSet;
use std::sync::Arc;

use futures::FutureExt;

use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, NoticeKind, SubsonicSettings};
use parking_lot::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::library::notice_playlist_worker::NoticeReconcile;
use crate::services::library::notice_queue::{NoticeEntry, NoticeState};
use octo_core::fingerprint::verification::{InconclusiveReason, VerificationResult};

fn track(id: &str) -> LibraryTrack {
    LibraryTrack::new(
        id,
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
        "rec-1",
        "flac",
        1000,
        330,
    )
}

fn with(id: &str, change: impl FnOnce(&mut LibraryTrack)) -> LibraryTrack {
    let mut track = track(id);
    change(&mut track);
    track
}

fn ids(tracks: &[LibraryTrack]) -> Vec<&str> {
    tracks.iter().map(|track| track.id.as_str()).collect()
}

// ---- DuplicateScanTests ------------------------------------------------------------------

#[test]
fn find_groups_same_recording_same_version_is_a_group_with_the_keeper_first() {
    let groups = DuplicateScanWorker::find_groups(&[
        with("b", |t| {
            t.suffix = "mp3".into();
            t.bit_rate = 320;
        }),
        with("a", |t| t.bit_rate = 1011),
    ]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key, "dup|a,b");
    assert_eq!(ids(&groups[0].tracks), ["a", "b"]);
}

#[test]
fn find_groups_same_recording_different_version_is_not_a_group() {
    assert!(
        DuplicateScanWorker::find_groups(&[track("a"), with("b", |t| t.title = "Teardrop (Live)".into())])
            .is_empty()
    );
}

#[test]
fn find_groups_different_artist_is_not_a_group() {
    assert!(
        DuplicateScanWorker::find_groups(&[track("a"), with("b", |t| t.artist = "Elbow".into())]).is_empty()
    );
}

#[test]
fn find_groups_different_recordings_are_not_a_group() {
    assert!(
        DuplicateScanWorker::find_groups(&[track("a"), with("b", |t| t.recording_id = "rec-2".into())])
            .is_empty()
    );
}

/// A guess about which files are the same song is a guess someone would act on.
#[test]
fn find_groups_no_recording_id_is_never_grouped() {
    assert!(
        DuplicateScanWorker::find_groups(&[
            with("a", |t| t.recording_id = String::new()),
            with("b", |t| t.recording_id = String::new()),
        ])
        .is_empty()
    );
}

#[test]
fn find_groups_three_copies_two_versions_groups_only_the_matching_pair() {
    let groups = DuplicateScanWorker::find_groups(&[
        track("a"),
        with("b", |t| t.title = "Teardrop (Mad Professor mix)".into()),
        with("c", |t| {
            t.title = "Teardrop - Remastered 2011".into();
            t.suffix = "mp3".into();
            t.bit_rate = 320;
        }),
    ]);

    assert_eq!(groups.len(), 1);
    assert_eq!(ids(&groups[0].tracks), ["a", "c"]);
}

fn coded(id: &str, suffix: &str, bit_rate: i32) -> LibraryTrack {
    with(id, |t| {
        t.suffix = suffix.into();
        t.bit_rate = bit_rate;
    })
}

#[test]
fn rank_for_keeping_lossless_then_bitrate_then_duration() {
    let ranked = DuplicateScanWorker::rank_for_keeping(&[
        coded("mp3-high", "mp3", 320),
        coded("aac", "m4a", 256),
        coded("alac", "m4a", 900),
        with("flac-odd-length", |t| t.duration = 400),
        with("flac", |t| t.duration = 330),
        coded("mp3-low", "mp3", 128),
    ]);

    assert_eq!(
        ids(&ranked),
        ["flac", "flac-odd-length", "alac", "mp3-high", "aac", "mp3-low"]
    );
}

// ---- fake lossless copies ---------------------------------------------------------------

/// `SpectrumAnalyzer.EstimateFor(16900)`.
const ABOUT_128: &str = "about 128 kbps MP3";

fn fake() -> SpectrumReport {
    SpectrumReport::new(
        SpectrumVerdict::LikelyLossy,
        44100,
        Some(16900.0),
        "a cliff",
        Some(ABOUT_128.to_string()),
    )
}

fn real() -> SpectrumReport {
    SpectrumReport::new(
        SpectrumVerdict::Genuine,
        44100,
        None,
        "audio up to the top of the band",
        None,
    )
}

#[test]
fn rank_for_keeping_a_transcoded_flac_never_outranks_a_genuine_one() {
    let ranked = DuplicateScanWorker::rank_for_keeping(&[
        with("fake", |t| {
            t.bit_rate = 1100;
            t.transcoded_from = Some(ABOUT_128.into());
        }),
        with("real", |t| t.bit_rate = 900),
        coded("mp3", "mp3", 320),
    ]);

    assert_eq!(ids(&ranked), ["real", "fake", "mp3"]);
}

#[tokio::test]
async fn check_transcodes_reranks_a_group_with_two_lossless_copies() {
    let groups = DuplicateScanWorker::find_groups(&[
        with("fake", |t| t.bit_rate = 1100),
        with("real", |t| t.bit_rate = 900),
        coded("mp3", "mp3", 320),
    ]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].tracks[0].id, "fake");
    let key = groups[0].key.clone();
    let asked = Mutex::new(Vec::new());

    let checked = DuplicateScanWorker::check_transcodes(groups, |track| {
        asked.lock().push(track.id.clone());
        let report = if track.id == "fake" { fake() } else { real() };
        async move { Some(report) }
    })
    .await;

    assert_eq!(checked.len(), 1);
    assert_eq!(checked[0].key, key);
    assert_eq!(ids(&checked[0].tracks), ["real", "fake", "mp3"]);
    assert_eq!(checked[0].tracks[1].transcoded_from.as_deref(), Some(ABOUT_128));
    // Only the lossless copies are decoded.
    let mut asked = asked.into_inner();
    asked.sort();
    assert_eq!(asked, ["fake", "real"]);
}

/// One lossless copy outranks the lossy ones whatever its spectrum says, so it is never decoded:
/// the check costs nothing outside groups where it can change the answer.
#[tokio::test]
async fn check_transcodes_leaves_a_group_with_one_lossless_copy_alone() {
    let groups = DuplicateScanWorker::find_groups(&[track("flac"), coded("mp3", "mp3", 320)]);
    let group = groups[0].clone();

    let checked = DuplicateScanWorker::check_transcodes(groups, |_| async {
        panic!("nothing should be decoded");
        #[allow(unreachable_code)]
        None
    })
    .await;

    assert_eq!(checked, [group]);
}

#[tokio::test]
async fn check_transcodes_a_copy_that_could_not_be_judged_counts_as_genuine() {
    let groups = DuplicateScanWorker::find_groups(&[
        with("a", |t| t.bit_rate = 1100),
        with("b", |t| t.bit_rate = 900),
    ]);

    let checked = DuplicateScanWorker::check_transcodes(groups, |track| {
        let report = (track.id != "a").then(|| SpectrumReport::unknown("no clear cutoff", 0));
        async move { report }
    })
    .await;

    assert_eq!(ids(&checked[0].tracks), ["a", "b"]);
    assert!(
        checked[0]
            .tracks
            .iter()
            .all(|track| track.transcoded_from.is_none())
    );
}

#[test]
fn sync_duplicates_says_which_copy_is_transcoded() {
    let queue = NoticeQueue::new();
    let group = DuplicateGroup {
        key: "dup|a,b".into(),
        tracks: vec![
            with("a", |t| t.bit_rate = 900),
            with("b", |t| {
                t.bit_rate = 1100;
                t.transcoded_from = Some("about 192 kbps".into());
            }),
        ],
    };

    queue.sync_duplicates(&[group], &["alice"], true);

    let entries = duplicates(&queue, "alice");
    assert_eq!(
        entries[1].reason,
        "FLAC, 1100 kbps, likely transcoded from about 192 kbps, also in the library as FLAC, 900 kbps"
    );
}

#[test]
fn parse_page_counts_every_row_but_keeps_only_tracks_with_a_recording_id() {
    let mut tracks = Vec::new();
    let root: Value = serde_json::from_str(
        r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[
          {"id":"a","title":"Teardrop","artist":"Massive Attack","album":"Mezzanine","musicBrainzId":"rec-1","suffix":"flac","bitRate":1011,"duration":330},
          {"id":"b","title":"Angel","artist":"Massive Attack","suffix":"mp3","bitRate":320,"duration":379}
        ]}}}"#,
    )
    .expect("json");

    assert_eq!(DuplicateScanWorker::parse_page(&root, &mut tracks), Ok(Some(2)));
    assert_eq!(
        tracks,
        [LibraryTrack::new(
            "a",
            "Teardrop",
            "Massive Attack",
            "Mezzanine",
            "rec-1",
            "flac",
            1011,
            330
        )]
    );
}

#[test]
fn parse_page_a_refusal_is_not_the_end() {
    for (body, expected) in [
        (
            r#"{"subsonic-response":{"status":"failed","error":{"code":40}}}"#,
            None,
        ),
        (
            r#"{"subsonic-response":{"status":"ok","searchResult3":{}}}"#,
            Some(0),
        ),
    ] {
        let root: Value = serde_json::from_str(body).expect("json");
        assert_eq!(
            DuplicateScanWorker::parse_page(&root, &mut Vec::new()),
            Ok(expected),
            "{body}"
        );
    }
}

/// The C# test's `LibraryNavidrome`: a login, and pages of `tracks` songs, the second failing
/// when asked.
struct LibraryNavidrome {
    tracks: usize,
    fail_second_page: bool,
    queries: Arc<Mutex<Vec<String>>>,
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query = request.url.query().unwrap_or("").to_string();
        self.queries.lock().push(query);
        let offset: usize = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "songOffset")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        if self.fail_second_page && offset > 0 {
            return ResponseTemplate::new(500);
        }
        let count = DuplicateScanWorker::PAGE_SIZE.min(self.tracks.saturating_sub(offset));
        let rows: Vec<String> = (offset..offset + count)
            .map(|i| {
                format!(
                    r#"{{"id":"t{i}","title":"Song {i}","artist":"A","musicBrainzId":"rec-{i}","suffix":"flac","bitRate":900,"duration":200}}"#
                )
            })
            .collect();
        ResponseTemplate::new(200).set_body_raw(
            format!(
                r#"{{"subsonic-response":{{"status":"ok","searchResult3":{{"song":[{}]}}}}}}"#,
                rows.join(",")
            ),
            "application/json",
        )
    }
}

async fn walk_worker(
    tracks: usize,
    fail_second_page: bool,
) -> (MockServer, DuplicateScanWorker, Arc<Mutex<Vec<String>>>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"token":"jwt","isAdmin":true,"username":"admin","subsonicToken":"tok","subsonicSalt":"salt"}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let queries = Arc::new(Mutex::new(Vec::new()));
    Mock::given(path("/rest/search3"))
        .respond_with(LibraryNavidrome {
            tracks,
            fail_second_page,
            queries: queries.clone(),
        })
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    }));
    let http = reqwest::Client::new();
    let worker = DuplicateScanWorker::new(
        Arc::new(NoticeQueue::new()),
        NavidromeIdentityService::new(settings.clone(), http.clone()),
        http,
        settings,
        None,
        None,
    );
    (server, worker, queries)
}

/// The same walk Symfonium makes: the empty query, page after page, until a short page.
#[tokio::test]
async fn walk_pages_through_the_library_until_a_short_page() {
    let (_server, worker, queries) = walk_worker(DuplicateScanWorker::PAGE_SIZE + 7, false).await;

    let (tracks, complete) = worker.walk().await.expect("walked");

    assert!(complete);
    assert_eq!(tracks.len(), DuplicateScanWorker::PAGE_SIZE + 7);
    let queries = queries.lock();
    assert_eq!(queries.len(), 2);
    assert!(queries.iter().all(|query| query.contains("query=%22%22")));
    assert!(queries[0].contains("u=admin"));
}

/// A walk cut short must not read as a library with nothing left in it.
#[tokio::test]
async fn walk_a_page_that_fails_is_incomplete() {
    let (_server, worker, _) = walk_worker(DuplicateScanWorker::PAGE_SIZE * 2, true).await;

    let (tracks, complete) = worker.walk().await.expect("walked");

    assert!(!complete);
    assert_eq!(tracks.len(), DuplicateScanWorker::PAGE_SIZE);
}

// ---- DuplicateNoticeTests ----------------------------------------------------------------

fn notice_track(id: &str, suffix: &str, bit_rate: i32) -> LibraryTrack {
    LibraryTrack::new(
        id,
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
        "rec-1",
        suffix,
        bit_rate,
        330,
    )
}

fn pair() -> DuplicateGroup {
    DuplicateGroup {
        key: "dup|a,b".into(),
        tracks: vec![notice_track("a", "flac", 1011), notice_track("b", "mp3", 320)],
    }
}

fn duplicates(queue: &NoticeQueue, user: &str) -> Vec<NoticeEntry> {
    let mut entries = queue.for_user(user, NoticeKind::Duplicates);
    entries.sort_by_key(|entry| entry.order);
    entries
}

#[test]
fn sync_duplicates_asks_each_allowed_user_about_every_copy_keeper_first() {
    let queue = NoticeQueue::new();

    assert_eq!(queue.sync_duplicates(&[pair()], &["alice", "bob", " "], true), 4);

    let entries = duplicates(&queue, "alice");
    assert_eq!(
        entries
            .iter()
            .filter_map(|e| e.navidrome_id.as_deref())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    for entry in &entries {
        assert_eq!(entry.state, NoticeState::Waiting);
        assert_eq!(entry.group_key.as_deref(), Some("dup|a,b"));
        assert_eq!(entry.local_path, "");
    }
    assert_eq!(entries[0].reason, "The best of 2 copies: FLAC, 1011 kbps");
    assert_eq!(
        entries[1].reason,
        "MP3, 320 kbps, also in the library as FLAC, 1011 kbps"
    );
    assert_eq!(duplicates(&queue, "bob").len(), 2);
}

#[test]
fn sync_duplicates_the_same_group_again_adds_nothing() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice"], true);

    assert_eq!(queue.sync_duplicates(&[pair()], &["alice"], true), 0);
}

/// One side went, so the pair Octo asked about is resolved.
#[test]
fn sync_duplicates_resolved_pair_expires() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice"], true);

    queue.sync_duplicates(&[], &["alice"], true);

    assert!(
        duplicates(&queue, "alice")
            .iter()
            .all(|e| e.state == NoticeState::Expired)
    );
}

#[test]
fn sync_duplicates_a_walk_that_did_not_finish_settles_nothing() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice"], true);

    queue.sync_duplicates(&[], &["alice"], false);

    assert!(
        duplicates(&queue, "alice")
            .iter()
            .all(|e| e.state == NoticeState::Waiting)
    );
}

#[test]
fn sync_duplicates_dismissed_group_stays_dismissed() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice", "bob"], true);
    for entry in duplicates(&queue, "alice") {
        queue.resolve(&entry.key, NoticeState::Dismissed);
    }

    queue.sync_duplicates(&[pair()], &["alice", "bob"], true);

    assert!(
        duplicates(&queue, "alice")
            .iter()
            .all(|e| e.state == NoticeState::Dismissed)
    );
    assert!(
        duplicates(&queue, "bob")
            .iter()
            .all(|e| e.state == NoticeState::Waiting)
    );
}

/// A copy that vanished for a while (a mount that dropped out) and came back is a pair again,
/// and is asked about again.
#[test]
fn sync_duplicates_an_expired_group_found_again_is_asked_again() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice"], true);
    queue.sync_duplicates(&[], &["alice"], true);

    assert_eq!(queue.sync_duplicates(&[pair()], &["alice"], true), 2);
    assert!(
        duplicates(&queue, "alice")
            .iter()
            .all(|e| e.state == NoticeState::Waiting)
    );
}

/// A duplicate is a Navidrome row, not a file Octo placed: nothing may expire it by looking for
/// a local path, and there is no id to look up.
#[test]
fn due_for_lookup_never_offers_a_duplicate() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice"], true);

    assert!(
        queue
            .due_for_lookup(Utc::now() + chrono::TimeDelta::days(30), 100)
            .is_empty()
    );
}

#[test]
fn mark_kept_on_one_copy_settles_that_persons_group_only() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice", "bob"], true);
    for user in ["alice", "bob"] {
        let keys: Vec<String> = duplicates(&queue, user).into_iter().map(|e| e.key).collect();
        queue.mark_queued(&keys);
    }

    assert!(queue.mark_kept("alice", "b").is_some());

    assert!(
        duplicates(&queue, "alice")
            .iter()
            .all(|e| e.state == NoticeState::Dismissed)
    );
    assert!(
        duplicates(&queue, "bob")
            .iter()
            .all(|e| e.state == NoticeState::Queued)
    );
}

/// A star or the Keep playlist cannot say which playlist it came from.
#[test]
fn mark_kept_a_track_in_review_and_duplicates_answers_both() {
    let queue = NoticeQueue::new();
    let unknown = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        fingerprint: Some("AQADtEqk".into()),
        duration_seconds: 330,
        ..Default::default()
    };
    queue.add_review(
        "alice",
        "/music/teardrop.flac",
        &Song {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            ..Default::default()
        },
        &unknown,
    );
    let review = NoticeQueue::review_key("alice", "/music/teardrop.flac");
    queue.set_navidrome_id(&review, "a");
    queue.mark_queued(&[review]);
    queue.sync_duplicates(&[pair()], &["alice"], true);

    let kept = queue.mark_kept("alice", "a");

    assert_eq!(kept.map(|k| k.kind), Some(NoticeKind::Review));
    assert_eq!(
        queue.for_user("alice", NoticeKind::Review)[0].state,
        NoticeState::Kept
    );
    assert!(
        duplicates(&queue, "alice")
            .iter()
            .all(|e| e.state == NoticeState::Dismissed)
    );
}

#[test]
fn mark_acted_on_one_copy_expires_the_rest_of_the_group_for_everyone() {
    let queue = NoticeQueue::new();
    queue.sync_duplicates(&[pair()], &["alice", "bob"], true);

    queue.mark_acted("b");

    for user in ["alice", "bob"] {
        let entries = duplicates(&queue, user);
        assert_eq!(entries[0].state, NoticeState::Expired, "{user}");
        assert_eq!(entries[1].state, NoticeState::Acted, "{user}");
    }
}

fn entry(id: &str, state: NoticeState, group: &str, order: i32) -> NoticeEntry {
    NoticeEntry {
        key: format!("k-{id}-{state:?}"),
        username: "alice".into(),
        kind: NoticeKind::Duplicates,
        navidrome_id: Some(id.into()),
        state,
        group_key: Some(group.into()),
        order,
        ..Default::default()
    }
}

/// A duplicate group is one question: taking either copy out answers it.
#[test]
fn plan_taking_one_copy_out_dismisses_the_group_and_takes_the_rest_off() {
    let plan = NoticeReconcile::plan(
        &[
            entry("a", NoticeState::Queued, "g", 0),
            entry("b", NoticeState::Queued, "g", 1),
        ],
        &HashSet::from(["b".to_string()]),
        10,
    );

    assert_eq!(plan.dismiss, ["k-a-Queued", "k-b-Queued"]);
    assert_eq!(plan.remove, ["b"]);
    assert!(plan.add.is_empty());
}

/// A group found again after an older one expired must keep its own tracks.
#[test]
fn plan_a_stale_entry_never_removes_a_track_an_open_question_still_needs() {
    let plan = NoticeReconcile::plan(
        &[
            entry("a", NoticeState::Expired, "old", 0),
            entry("a", NoticeState::Queued, "new", 0),
            entry("c", NoticeState::Queued, "new", 1),
        ],
        &HashSet::from(["a".to_string(), "c".to_string()]),
        10,
    );

    assert!(plan.remove.is_empty());
    assert!(plan.dismiss.is_empty());
}

// ---- Rust-only ---------------------------------------------------------------------------

#[test]
fn lossless_files_are_told_apart_by_suffix_and_an_alac_bitrate() {
    for (suffix, rate, expected) in [
        ("FLAC", 0, true),
        ("dsf", 0, true),
        ("m4a", 900, true),
        ("m4a", 256, false),
        ("mp3", 1411, false),
    ] {
        assert_eq!(is_lossless_file(suffix, rate), expected, "{suffix} {rate}");
    }
}

#[test]
fn a_page_that_is_not_an_object_reads_as_the_c_sharp_threw() {
    let root: Value =
        serde_json::from_str(r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[1]}}}"#)
            .expect("json");
    assert!(DuplicateScanWorker::parse_page(&root, &mut Vec::new()).is_err());
    let root: Value = serde_json::from_str("[]").expect("json");
    assert!(DuplicateScanWorker::parse_page(&root, &mut Vec::new()).is_err());
}

#[test]
fn a_scan_request_is_stored_once() {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let http = reqwest::Client::new();
    let worker = DuplicateScanWorker::new(
        Arc::new(NoticeQueue::new()),
        NavidromeIdentityService::new(settings.clone(), http.clone()),
        http,
        settings,
        None,
        None,
    );
    worker.request_scan();
    worker.request_scan();
    assert!(worker.requested.notified().now_or_never().is_some());
    assert!(worker.requested.notified().now_or_never().is_none());
    assert!(worker.last_result().is_none());
}
