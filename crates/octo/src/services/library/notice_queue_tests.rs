//! `NoticeQueueTests.cs` (`NoticeQueueTests`): what Octo has asked each person, and what they
//! answered (#47). The reconcile tests of the same file are in `notice_playlist_worker_tests.rs`.

use std::path::PathBuf;

use octo_core::fingerprint::acoust_id_client::AcoustIdRecording;

use super::*;

fn song() -> Song {
    Song {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        album: "Mezzanine".into(),
        ..Default::default()
    }
}

fn verdict(reason: InconclusiveReason) -> VerificationResult {
    VerificationResult {
        reason,
        fingerprint: Some("AQADtEqk".into()),
        duration_seconds: 330,
        ..Default::default()
    }
}

fn only(queue: &NoticeQueue, user: &str) -> NoticeEntry {
    let mut entries = queue.for_user(user, NoticeKind::Review);
    assert_eq!(entries.len(), 1, "one entry for {user}");
    entries.remove(0)
}

fn asked(queue: &NoticeQueue, user: &str, path: &str, id: &str) -> String {
    queue.add_review(user, path, &song(), &verdict(InconclusiveReason::NoEntry));
    let key = NoticeQueue::review_key(user, path);
    queue.set_navidrome_id(&key, id);
    queue.mark_queued(std::slice::from_ref(&key));
    key
}

#[test]
fn add_review_says_why_octo_is_asking() {
    for (reason, expected) in [
        (
            InconclusiveReason::NoEntry,
            "AcoustID has never heard this recording",
        ),
        (
            InconclusiveReason::BelowThreshold,
            "AcoustID was not sure what this is",
        ),
    ] {
        let queue = NoticeQueue::new();
        assert!(queue.add_review("alice", "/music/a.flac", &song(), &verdict(reason)));
        let entry = only(&queue, "alice");
        assert_eq!(entry.reason, expected, "{reason:?}");
        assert_eq!(entry.state, NoticeState::Waiting);
        assert_eq!(entry.file_format.as_deref(), Some("flac"));
    }
}

#[test]
fn add_review_a_source_disagreement_names_what_acoust_id_heard() {
    let queue = NoticeQueue::new();
    let disagreed = VerificationResult {
        matched_artist: Some("Elizabeth Fraser".into()),
        matched_title: Some("Song to the Siren".into()),
        ..verdict(InconclusiveReason::SourceDisagreed)
    };
    queue.add_review("alice", "/music/a.mp3", &song(), &disagreed);
    assert_eq!(
        only(&queue, "alice").reason,
        "AcoustID thinks this is 'Elizabeth Fraser - Song to the Siren'"
    );
}

/// A dismissal would otherwise be undone by the next download of the same file.
#[test]
fn add_review_a_file_already_asked_about_is_never_asked_again_in_any_state() {
    let queue = NoticeQueue::new();
    let key = asked(&queue, "alice", "/music/a.flac", "nd1");
    queue.resolve(&key, NoticeState::Dismissed);

    assert!(!queue.add_review(
        "Alice",
        "/music/a.flac",
        &song(),
        &verdict(InconclusiveReason::NoEntry)
    ));
    assert_eq!(only(&queue, "alice").state, NoticeState::Dismissed);
}

#[test]
fn is_queued_is_per_person_and_only_while_asked() {
    let queue = NoticeQueue::new();
    let key = asked(&queue, "alice", "/music/a.flac", "nd1");

    assert!(queue.is_queued("ALICE", "nd1"));
    assert!(!queue.is_queued("bob", "nd1"));
    assert!(!queue.is_queued("alice", "nd2"));

    queue.resolve(&key, NoticeState::Dismissed);
    assert!(!queue.is_queued("alice", "nd1"));
}

#[test]
fn mark_kept_not_asked_about_this_track_is_null() {
    let queue = NoticeQueue::new();
    asked(&queue, "alice", "/music/a.flac", "nd1");

    assert!(queue.mark_kept("alice", "nd2").is_none());
    assert!(queue.mark_kept("bob", "nd1").is_none());
    assert_eq!(only(&queue, "alice").state, NoticeState::Queued);
}

/// A download nobody requested is asked of every allowed user. The question was about the
/// file, so one person's Keep answers it for all of them, and only their entry keeps the
/// fingerprint: the same fingerprint sent twice would count as two confirmations.
#[test]
fn mark_kept_answers_everyone_asked_about_the_file_but_submits_once() {
    let queue = NoticeQueue::new();
    asked(&queue, "alice", "/music/a.flac", "nd1");
    asked(&queue, "bob", "/music/a.flac", "nd1");

    let kept = queue.mark_kept("bob", "nd1");

    assert_eq!(kept.map(|k| k.username).as_deref(), Some("bob"));
    assert_eq!(only(&queue, "alice").state, NoticeState::Kept);
    assert_eq!(only(&queue, "bob").state, NoticeState::Kept);
    let waiting = queue.awaiting_submission();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].username, "bob");
}

#[test]
fn resolve_anything_but_keep_drops_the_fingerprint() {
    let queue = NoticeQueue::new();
    let key = asked(&queue, "alice", "/music/a.flac", "nd1");

    queue.resolve(&key, NoticeState::Dismissed);

    assert!(only(&queue, "alice").fingerprint.is_none());
    assert!(queue.awaiting_submission().is_empty());
}

#[test]
fn mark_acted_settles_open_questions_for_everyone() {
    let queue = NoticeQueue::new();
    asked(&queue, "alice", "/music/a.flac", "nd1");
    asked(&queue, "bob", "/music/a.flac", "nd1");

    queue.mark_acted("nd1");

    assert_eq!(only(&queue, "alice").state, NoticeState::Acted);
    assert_eq!(only(&queue, "bob").state, NoticeState::Acted);
    assert!(queue.awaiting_submission().is_empty());
}

/// Taking a track out of Review and dropping it into Delete is one answer, Delete, even though
/// the sweep saw the removal first.
#[test]
fn mark_acted_shortly_after_a_dismissal_records_the_action() {
    let queue = NoticeQueue::new();
    let key = asked(&queue, "alice", "/music/a.flac", "nd1");
    queue.resolve(&key, NoticeState::Dismissed);

    queue.mark_acted("nd1");

    assert_eq!(only(&queue, "alice").state, NoticeState::Acted);
}

#[test]
fn mark_submitted_sent_or_refused_either_way_the_fingerprint_goes() {
    let queue = NoticeQueue::new();
    asked(&queue, "alice", "/music/a.flac", "nd1");
    asked(&queue, "bob", "/music/b.flac", "nd2");
    queue.mark_kept("alice", "nd1");
    queue.mark_kept("bob", "nd2");

    queue.mark_submitted(&[NoticeQueue::review_key("alice", "/music/a.flac")], true);
    queue.mark_submitted(&[NoticeQueue::review_key("bob", "/music/b.flac")], false);

    assert!(only(&queue, "alice").submitted);
    assert!(!only(&queue, "bob").submitted);
    assert!(only(&queue, "alice").fingerprint.is_none());
    assert!(only(&queue, "bob").fingerprint.is_none());
    assert!(queue.awaiting_submission().is_empty());
}

#[test]
fn defer_lookup_backs_off_and_due_for_lookup_waits_for_it() {
    let queue = NoticeQueue::new();
    queue.add_review(
        "alice",
        "/music/a.flac",
        &song(),
        &verdict(InconclusiveReason::NoEntry),
    );
    let key = NoticeQueue::review_key("alice", "/music/a.flac");
    let now = Utc::now() + TimeDelta::seconds(1);

    assert_eq!(queue.due_for_lookup(now, 10).len(), 1);
    queue.defer_lookup(&key, now);
    assert!(queue.due_for_lookup(now, 10).is_empty());
    assert_eq!(queue.due_for_lookup(now + TimeDelta::minutes(1), 10).len(), 1);

    queue.defer_lookup(&key, now);
    assert!(queue.due_for_lookup(now + TimeDelta::minutes(1), 10).is_empty());
    assert_eq!(queue.due_for_lookup(now + TimeDelta::minutes(2), 10).len(), 1);

    queue.set_navidrome_id(&key, "nd1");
    assert!(queue.due_for_lookup(now + TimeDelta::days(1), 10).is_empty());
}

/// The file is bounded, but an open question is never the thing forgotten.
#[test]
fn trim_drops_the_oldest_settled_entries_never_an_open_one() {
    let queue = NoticeQueue::new();
    for i in 0..NoticeQueue::MAX_ENTRIES {
        let path = format!("/music/{i}.flac");
        queue.add_review("alice", &path, &song(), &verdict(InconclusiveReason::NoEntry));
        if i >= 10 {
            queue.resolve(&NoticeQueue::review_key("alice", &path), NoticeState::Dismissed);
        }
    }

    queue.add_review(
        "alice",
        "/music/new.flac",
        &song(),
        &verdict(InconclusiveReason::NoEntry),
    );

    let entries = queue.for_user("alice", NoticeKind::Review);
    assert_eq!(entries.len(), NoticeQueue::MAX_ENTRIES);
    assert_eq!(entries.iter().filter(|entry| entry.is_open()).count(), 11);
    assert!(!entries.iter().any(|entry| entry.local_path == "/music/10.flac"));
}

#[test]
fn entries_survive_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("notices").join("notice-queue.json");
    {
        let queue = NoticeQueue::with_path(Some(path.clone()));
        asked(&queue, "alice", "/music/a.flac", "nd1");
        assert!(queue.flush());
    }

    let reopened = NoticeQueue::with_path(Some(path));
    let entry = only(&reopened, "alice");
    assert_eq!(entry.state, NoticeState::Queued);
    assert_eq!(entry.navidrome_id.as_deref(), Some("nd1"));
    assert_eq!(entry.cause, InconclusiveReason::NoEntry);
    assert!(reopened.is_queued("alice", "nd1"));
}

/// The file holds what people already answered, so it is set aside, never overwritten.
#[test]
fn load_a_corrupt_file_is_kept_aside_and_the_queue_starts_empty() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("notice-queue.json");
    std::fs::write(&path, "{ not json").expect("written");

    let queue = NoticeQueue::with_path(Some(path));

    assert!(queue.recent(200).is_empty());
    let aside: Vec<String> = std::fs::read_dir(dir.path())
        .expect("listed")
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| name.starts_with("notice-queue.json.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
}

// ---- Rust-only ---------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state")
        .join(name)
}

/// The state file reads and writes back byte for byte: names for enums, `DateTime.MinValue`
/// without a `Z`, seven-digit fractions, and `IsOpen` left out.
#[test]
fn the_notice_queue_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read_to_string(fixture("notice-queue.json")).expect("the fixture");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("notice-queue.json");
    std::fs::write(&path, &original).expect("copied");

    let queue = NoticeQueue::with_path(Some(path.clone()));
    let first = queue
        .recent(200)
        .into_iter()
        .find(|e| e.kind == NoticeKind::Review)
        .expect("a review");
    assert_eq!(first.cause, InconclusiveReason::SourceDisagreed);
    // The same id again changes nothing but marks the queue for a write.
    queue.set_navidrome_id(&first.key, first.navidrome_id.as_deref().expect("an id"));
    assert!(queue.flush());
    assert_eq!(std::fs::read_to_string(&path).expect("written"), original);
}

/// The converter read names in any case, and numbers.
#[test]
fn enums_are_read_by_name_in_any_case_or_by_number() {
    let entries: Vec<NoticeEntry> = serde_json::from_str(
        r#"[{"Key":"k","Kind":"duplicates","State":3,"Cause":" lengthoff ","Origin":"1"}]"#,
    )
    .expect("reads");
    assert_eq!(entries[0].kind, NoticeKind::Duplicates);
    assert_eq!(entries[0].state, NoticeState::Acted);
    assert_eq!(entries[0].cause, InconclusiveReason::LengthOff);
    assert_eq!(entries[0].origin, NoticeOrigin::LibrarySweep);
    assert!(serde_json::from_str::<Vec<NoticeEntry>>(r#"[{"State":"Sideways"}]"#).is_err());
}

#[test]
fn a_length_question_says_both_lengths() {
    let queue = NoticeQueue::new();
    let mut recording =
        AcoustIdRecording::new("rec", "Teardrop", vec!["Massive Attack".to_string()], None, None);
    recording.duration_seconds = Some(3725);
    let length = VerificationResult {
        duration_seconds: 120,
        r#match: Some(recording),
        ..verdict(InconclusiveReason::LengthOff)
    };
    queue.add_review_from(
        "bob",
        "/music/a.FLAC",
        &song(),
        &length,
        NoticeOrigin::LibrarySweep,
    );
    let entry = only(&queue, "bob");
    assert_eq!(
        entry.reason,
        "It runs 2:00, but the recording it matched runs 1:02:05"
    );
    assert_eq!(entry.file_format.as_deref(), Some("flac"));
    assert!(
        entry.fingerprint.is_none(),
        "a library question keeps no fingerprint"
    );
    assert_eq!(queue.open_count(NoticeOrigin::LibrarySweep), 1);
    assert_eq!(queue.open_count(NoticeOrigin::Download), 0);
    assert!(queue.reviewed_paths().contains("/music/a.FLAC"));
}

#[test]
fn recent_is_newest_first() {
    let queue = NoticeQueue::new();
    asked(&queue, "alice", "/music/a.flac", "nd1");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let key = asked(&queue, "alice", "/music/b.flac", "nd2");
    let recent = queue.recent(1);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].key, key);
}
