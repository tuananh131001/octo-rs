//! GenreBackfillUndoTests, and Rust-only tests of the walk (the C# tests reached it only through
//! the admin endpoints, which are 6-B's).
//!
//! Undo is the only thing that makes an apply reversible, so what it keeps in the journal is
//! pinned here. Before this, any undo cleared the whole journal when it finished, including one
//! that was cancelled half way, hit an unreadable file, or ran while the music folder's mount
//! was down, and the undo for everything it had not reached was gone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use octo_core::settings::{AppSettings, GenreMappingSettings, GenreSettings, SettingsStore};
use octo_media::tags::TagFile;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::local::LocalSongMapping;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::test_audio::audio_or_skip;

struct Dir {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Dir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        Self {
            root: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    fn path(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }
}

fn entry(path: &str, before: &str, after: &str, minute: u32) -> GenreJournalEntry {
    GenreJournalEntry::new(
        path,
        vec![before.to_string()],
        vec![after.to_string()],
        Utc.with_ymd_and_hms(2026, 9, 22, 12, minute, 0).unwrap(),
        "run",
    )
}

fn settings(genre: GenreSettings, music: Option<&Path>) -> Arc<SettingsStore> {
    let store = SettingsStore::for_tests(AppSettings {
        genre,
        ..AppSettings::default()
    });
    if let Some(music) = music {
        store.set_raw("Library:DownloadPath", Some(&music.to_string_lossy()));
    }
    Arc::new(store)
}

fn worker(store: &Arc<GenreBackfillStore>, journal: &Arc<GenreBackfillJournal>) -> Arc<GenreBackfillWorker> {
    Arc::new(GenreBackfillWorker::new(
        store.clone(),
        journal.clone(),
        settings(GenreSettings::default(), None),
        None,
    ))
}

/// Starts the worker, hands it `request`, waits for the run to leave Idle and Running (Replace
/// flips the status to Running as it starts), and stops the worker.
async fn run_until_idle(
    worker: &Arc<GenreBackfillWorker>,
    store: &GenreBackfillStore,
    request: GenreBackfillRequest,
) {
    let stopping = CancellationToken::new();
    let task = tokio::spawn(worker.clone().run(stopping.clone()));
    assert!(worker.try_enqueue(request), "the request was taken");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline
        && (worker.pending.load(Ordering::SeqCst)
            || matches!(
                store.read(|run| run.status),
                GenreBackfillStatus::Idle | GenreBackfillStatus::Running
            ))
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    stopping.cancel();
    task.await
        .expect("the worker ends")
        .expect("the worker ends cleanly");
}

fn undo() -> GenreBackfillRequest {
    GenreBackfillRequest::undo()
}

/// Undo_KeepsEntriesItCouldNotRestore: a file that cannot be read and a file that is not there
/// are both left for a later undo rather than dropped.
#[tokio::test]
async fn undo_keeps_entries_it_could_not_restore() {
    let dir = Dir::new();
    let unreadable = dir.path("not-audio.xyz");
    std::fs::write(&unreadable, [0u8; 16]).expect("written");
    let missing = dir.path("moved-away.flac");

    let store = Arc::new(GenreBackfillStore::new(Some(dir.root.join("state.json"))));
    let journal = Arc::new(GenreBackfillJournal::new(Some(dir.root.join("journal.jsonl"))));
    journal.append(&entry(&missing, "Rock", "Metal", 1));
    journal.append(&entry(&unreadable, "Jazz", "Blues", 2));

    let worker = worker(&store, &journal);
    run_until_idle(&worker, &store, undo()).await;

    assert_eq!(store.current().status, GenreBackfillStatus::Completed);
    let mut left: Vec<String> = journal.read_all().into_iter().map(|entry| entry.path).collect();
    left.sort();
    let mut expected = vec![missing, unreadable];
    expected.sort();
    assert_eq!(left, expected);
    assert!(
        store
            .current()
            .reason
            .unwrap_or_default()
            .contains("stay in the journal"),
        "the reason says so"
    );
}

/// Undo_RestoresAWritableFile_AndDropsItsEntry. The C# test tagged a 44-byte WAV and skipped
/// in effect when TagLib could not; here the song is a real MP3 made by ffmpeg.
#[tokio::test]
async fn undo_restores_a_writable_file_and_drops_its_entry() {
    let bytes = audio_or_skip!("mp3");
    let dir = Dir::new();
    let song = dir.path("tiny.mp3");
    std::fs::write(&song, bytes).expect("written");
    {
        let mut file = TagFile::open(&song).expect("opens");
        file.set_genres(&["Metal".to_string()]);
        file.save().expect("saved");
    }

    let store = Arc::new(GenreBackfillStore::new(Some(dir.root.join("state.json"))));
    let journal = Arc::new(GenreBackfillJournal::new(Some(dir.root.join("journal.jsonl"))));
    journal.append(&entry(&song, "Rock", "Metal", 1));

    let worker = worker(&store, &journal);
    run_until_idle(&worker, &store, undo()).await;

    assert!(!journal.exists());
    assert_eq!(TagFile::open(&song).expect("opens").genres(), ["Rock"]);
    assert_eq!(store.current().changed, 1);
}

/// TryEnqueue_RefusesWhileARequestIsWaiting: a bounded channel in DropWrite mode reports a
/// dropped write as a success, so a second request while the first was still enumerating used
/// to come back 202 and then vanish, or run afterwards without a second confirmation.
#[test]
fn try_enqueue_refuses_while_a_request_is_waiting() {
    let store = Arc::new(GenreBackfillStore::new(None));
    let worker = worker(&store, &Arc::new(GenreBackfillJournal::new(None)));

    assert!(worker.try_enqueue(GenreBackfillRequest::new(GenreBackfillScope::OctoDownloads, true)));
    assert!(!worker.try_enqueue(GenreBackfillRequest::new(
        GenreBackfillScope::OctoDownloads,
        false
    )));
}

#[test]
fn remaining_keeps_failed_skipped_and_unreached_entries_oldest_first() {
    let newest_first = [
        entry("/m/c.flac", "C0", "C1", 3), // restored
        entry("/m/b.flac", "B0", "B1", 2), // failed
        entry("/m/a.flac", "A0", "A1", 1), // never reached
    ];
    let outcomes = HashMap::from([(0, Some(true)), (1, Some(false))]);

    let left = GenreBackfillWorker::remaining(&newest_first, &outcomes);

    let paths: Vec<&str> = left.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, ["/m/a.flac", "/m/b.flac"]);
}

/// A file changed twice: the newer entry failed, the older one restored the original. Keeping
/// the newer one would put the in-between genre back on the next undo.
#[test]
fn remaining_drops_newer_entries_an_older_restore_superseded() {
    let newest_first = [
        entry("/m/x.flac", "Mid", "Final", 2),    // failed
        entry("/m/x.flac", "Original", "Mid", 1), // restored
    ];
    let outcomes = HashMap::from([(0, Some(false)), (1, Some(true))]);

    assert!(GenreBackfillWorker::remaining(&newest_first, &outcomes).is_empty());
}

#[test]
fn rewrite_round_trips_oldest_first() {
    let dir = Dir::new();
    let journal = GenreBackfillJournal::new(Some(dir.root.join("journal.jsonl")));
    journal.rewrite(&[
        entry("/m/old.flac", "A", "B", 1),
        entry("/m/new.flac", "C", "D", 2),
    ]);

    // ReadAll is newest first.
    let paths: Vec<String> = journal.read_all().into_iter().map(|entry| entry.path).collect();
    assert_eq!(paths, ["/m/new.flac", "/m/old.flac"]);

    journal.rewrite(&[]);
    assert!(!journal.exists());
}

#[test]
fn hash_settings_is_stable() {
    assert_eq!(
        GenreBackfillWorker::hash_settings(&GenreSettings::default()),
        GenreBackfillWorker::hash_settings(&GenreSettings::default())
    );
}

/// How long a run keeps trying is not what it writes, so changing it must not withdraw a
/// preview's Apply.
#[test]
fn hash_settings_ignores_the_failure_ceiling() {
    assert_eq!(
        GenreBackfillWorker::hash_settings(&GenreSettings::default()),
        GenreBackfillWorker::hash_settings(&GenreSettings {
            backfill_max_consecutive_failures: 3,
            ..GenreSettings::default()
        })
    );
}

/// Journal paths come from one walk; on Linux a.flac and A.flac are two files.
#[test]
fn remaining_compares_paths_exactly() {
    let newest_first = [
        entry("/m/A.flac", "X", "Y", 2), // failed
        entry("/m/a.flac", "P", "Q", 1), // restored
    ];
    let outcomes = HashMap::from([(0, Some(false)), (1, Some(true))]);

    let left = GenreBackfillWorker::remaining(&newest_first, &outcomes);
    let paths: Vec<&str> = left.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, ["/m/A.flac"]);
}

fn garage() -> GenreSettings {
    let mut edited = GenreSettings::default();
    edited.mappings.push(GenreMappingSettings {
        pattern: "garage".into(),
        genre: "Rock".into(),
        ..GenreMappingSettings::default()
    });
    edited
}

#[test]
fn hash_settings_changes_with_a_mapping() {
    assert_ne!(
        GenreBackfillWorker::hash_settings(&GenreSettings::default()),
        GenreBackfillWorker::hash_settings(&garage())
    );
}

/// Rust-only: the hashes the C# `HashSettings` gives for the same settings (run on .NET 9), so
/// a run the C# build recorded still resumes under the Rust one.
#[test]
fn hash_settings_matches_the_csharp_hashes() {
    let mut rich = GenreSettings {
        enabled: true,
        fallback: octo_core::settings::GenreFallbackSource::MusicBrainz,
        max_genres: 3,
        on_empty: octo_core::settings::GenreEmptyBehavior::Unknown,
        unknown_label: "Ünbekannt <&>".into(),
        blocklist: vec!["People & Blogs".into(), "Mötley".into()],
        backfill_extensions: vec!["FLAC".into(), ".mp3".into()],
        ..GenreSettings::default()
    };
    rich.mappings.push(GenreMappingSettings {
        id: "x1".into(),
        pattern: "hip hop".into(),
        genre: "Hip-Hop/Rap".into(),
        match_mode: octo_core::settings::GenreMatchMode::Exact,
        enabled: false,
    });
    let cases = [
        ("default", GenreSettings::default(), "5BCCD5FD6152B8B3"),
        ("a mapping", garage(), "9C05A11CD29110F8"),
        ("every field, escaped", rich, "EF59CB540DF26B84"),
    ];
    for (name, settings, expected) in cases {
        assert_eq!(GenreBackfillWorker::hash_settings(&settings), expected, "{name}");
    }
}

// ---- Rust-only: the walk -------------------------------------------------------------------

/// A library of MP3s tagged with these genre frames, under `<dir>/music`.
fn library(dir: &Dir, bytes: &[u8], songs: &[(&str, &[&str])]) -> PathBuf {
    let music = dir.root.join("music");
    std::fs::create_dir_all(music.join("sub")).expect("created");
    for (name, genres) in songs {
        let path = music.join(name);
        std::fs::write(&path, bytes).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_genres(&genres.iter().map(|genre| genre.to_string()).collect::<Vec<_>>());
        file.save().expect("saved");
    }
    music
}

fn genres_of(path: &Path) -> Vec<String> {
    TagFile::open(path).expect("opens").genres()
}

fn walk_worker(
    dir: &Dir,
    genre: GenreSettings,
    music: &Path,
    library: Option<Arc<dyn ILocalLibraryService>>,
) -> (
    Arc<GenreBackfillWorker>,
    Arc<GenreBackfillStore>,
    Arc<GenreBackfillJournal>,
) {
    let store = Arc::new(GenreBackfillStore::new(Some(
        dir.root.join("genre-backfill.json"),
    )));
    let journal = Arc::new(GenreBackfillJournal::new(Some(
        dir.root.join("genre-backfill-journal.jsonl"),
    )));
    let worker = Arc::new(GenreBackfillWorker::new(
        store.clone(),
        journal.clone(),
        settings(genre, Some(music)),
        library,
    ));
    (worker, store, journal)
}

/// A preview writes nothing and lists the changes; Apply writes them in ordinal order with a
/// journal line each, asks for a scan, and Undo puts every frame back.
#[tokio::test]
async fn a_whole_library_preview_apply_and_undo() {
    let bytes = audio_or_skip!("mp3");
    let dir = Dir::new();
    let music = library(
        &dir,
        &bytes,
        &[
            ("b.mp3", &["UK Garage"]),
            ("a.mp3", &["People & Blogs"]),
            ("sub/c.mp3", &["Rock"]),
        ],
    );
    std::fs::write(music.join("notes.txt"), "not a song").expect("written");
    let fake = Arc::new(FakeLocalLibrary::default());
    let (worker, store, journal) = walk_worker(&dir, garage(), &music, Some(fake.clone()));

    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::WholeLibrary, true),
    )
    .await;
    let run = store.current();
    assert_eq!(run.status, GenreBackfillStatus::Completed);
    assert_eq!((run.total, run.processed, run.changed, run.cleared), (3, 3, 2, 1));
    let rows: Vec<(String, Vec<String>, String)> = run
        .preview
        .iter()
        .map(|row| {
            (
                Path::new(&row.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                row.after.clone(),
                row.action.clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("a.mp3".to_string(), vec![], "Clear".to_string()),
            ("b.mp3".to_string(), vec!["Rock".to_string()], "Write".to_string())
        ]
    );
    assert_eq!(
        run.settings_hash.as_deref(),
        Some(GenreBackfillWorker::hash_settings(&garage()).as_str())
    );
    assert_eq!(genres_of(&music.join("b.mp3")), ["UK Garage"]);
    assert!(!journal.exists());
    assert_eq!(fake.scans(), 0);

    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::WholeLibrary, false),
    )
    .await;
    assert_eq!(store.current().changed, 2);
    assert_eq!(genres_of(&music.join("b.mp3")), ["Rock"]);
    assert!(genres_of(&music.join("a.mp3")).is_empty());
    assert_eq!(journal.read_all().len(), 2);
    assert_eq!(fake.scans(), 1);
    // The state file was written on Replace, and the flush writes the rest.
    store.flush();
    let on_disk = std::fs::read_to_string(dir.root.join("genre-backfill.json")).expect("written");
    assert!(on_disk.contains("\"Status\":2"), "{on_disk}");

    run_until_idle(&worker, &store, undo()).await;
    assert_eq!(store.current().status, GenreBackfillStatus::Completed);
    assert_eq!(
        store.current().reason.as_deref(),
        Some("Restored the genre frame on 2 file(s).")
    );
    assert_eq!(genres_of(&music.join("b.mp3")), ["UK Garage"]);
    assert_eq!(genres_of(&music.join("a.mp3")), ["People & Blogs"]);
    assert!(!journal.exists());
    assert_eq!(fake.scans(), 2);
}

/// A cancelled run resumes where it stopped under the same settings, and starts afresh once
/// the rules changed.
#[tokio::test]
async fn a_cancelled_run_resumes_unless_the_rules_changed() {
    let bytes = audio_or_skip!("mp3");
    let dir = Dir::new();
    let music = library(
        &dir,
        &bytes,
        &[("a.mp3", &["UK Garage"]), ("b.mp3", &["Garage Rock"])],
    );
    let (worker, store, _journal) = walk_worker(&dir, garage(), &music, None);
    let queue: Vec<String> = ["a.mp3", "b.mp3"]
        .iter()
        .map(|name| music.join(name).to_string_lossy().into_owned())
        .collect();
    store.replace(GenreBackfillRun {
        run_id: "r1".into(),
        status: GenreBackfillStatus::Cancelled,
        scope: GenreBackfillScope::WholeLibrary,
        dry_run: true,
        total: 2,
        processed: 1,
        cursor: 1,
        queue: queue.clone(),
        settings_hash: Some(GenreBackfillWorker::hash_settings(&garage())),
        ..GenreBackfillRun::default()
    });

    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::WholeLibrary, true),
    )
    .await;
    let run = store.current();
    assert_eq!((run.run_id.as_str(), run.processed, run.changed), ("r1", 2, 1));
    assert_eq!(run.preview[0].path, queue[1]);

    // Another table: the hash differs, so the run starts again from the first file.
    store.update(|run| {
        run.status = GenreBackfillStatus::Cancelled;
        run.cursor = 1;
        run.settings_hash = Some("0000000000000000".into());
    });
    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::WholeLibrary, true),
    )
    .await;
    let run = store.current();
    assert_ne!(run.run_id, "r1");
    assert_eq!((run.processed, run.changed), (2, 2));
}

/// Octo's downloads come from the mappings (existing files only, once each), and without a
/// library service the run fails with the container's message, as the C# did.
#[tokio::test]
async fn octo_downloads_walks_the_mappings_and_fails_without_a_library() {
    let bytes = audio_or_skip!("mp3");
    let dir = Dir::new();
    let music = library(&dir, &bytes, &[("a.mp3", &["UK Garage"])]);
    let song = music.join("a.mp3").to_string_lossy().into_owned();
    let fake = Arc::new(FakeLocalLibrary::default());
    for (path, title) in [
        (song.as_str(), "A"),
        (song.as_str(), "A again"),
        ("/nowhere/x.mp3", "X"),
    ] {
        let mapping = LocalSongMapping {
            local_path: path.to_string(),
            ..LocalSongMapping::default()
        };
        fake.by_tags
            .lock()
            .push(("Artist".into(), title.into(), None, mapping));
    }
    let (worker, store, _journal) = walk_worker(&dir, garage(), &music, Some(fake));
    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::OctoDownloads, true),
    )
    .await;
    assert_eq!(store.current().queue, [song]);

    // A fresh config folder: this one holds a run the store would resume.
    let other = Dir::new();
    let (worker, store, _journal) = walk_worker(&other, garage(), &music, None);
    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::OctoDownloads, true),
    )
    .await;
    let run = store.current();
    assert_eq!(run.status, GenreBackfillStatus::Failed);
    assert_eq!(run.reason.as_deref(), Some(NO_LIBRARY_SERVICE));
}

/// Files that cannot be written stop the run at the consecutive-failure ceiling; a file that is
/// not audio is skipped, not failed.
#[tokio::test]
async fn unwritable_files_stop_the_run_and_strangers_are_skipped() {
    let dir = Dir::new();
    let music = dir.root.join("music");
    std::fs::create_dir_all(&music).expect("created");
    // MP3s that cannot be parsed are skipped; a ".flac" that is a folder cannot be opened.
    std::fs::write(music.join("junk.mp3"), b"not audio").expect("written");
    for name in ["x1.flac", "x2.flac", "x3.flac"] {
        std::fs::create_dir_all(music.join(name)).expect("created");
    }
    let genre = GenreSettings {
        backfill_max_consecutive_failures: 2,
        ..garage()
    };
    let (worker, store, _journal) = walk_worker(&dir, genre, &music, None);
    store.replace(GenreBackfillRun {
        run_id: "r".into(),
        status: GenreBackfillStatus::Interrupted,
        scope: GenreBackfillScope::WholeLibrary,
        dry_run: false,
        queue: ["junk.mp3", "x1.flac", "x2.flac", "x3.flac"]
            .iter()
            .map(|name| music.join(name).to_string_lossy().into_owned())
            .collect(),
        total: 4,
        ..GenreBackfillRun::default()
    });
    run_until_idle(
        &worker,
        &store,
        GenreBackfillRequest::new(GenreBackfillScope::WholeLibrary, false),
    )
    .await;
    let run = store.current();
    assert_eq!(run.status, GenreBackfillStatus::Failed);
    assert_eq!((run.skipped, run.failed, run.cursor), (1, 2, 3));
    assert_eq!(
        run.reason.as_deref(),
        Some("2 files in a row could not be written. Is the music directory read-only?")
    );
}
