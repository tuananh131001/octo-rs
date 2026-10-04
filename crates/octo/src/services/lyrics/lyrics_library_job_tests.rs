//! LyricsChoiceTests' library job tests (Job_*), and the store and worker around them. The C#
//! tests wrote tags into an MP3 fixture with TagLib; here the songs' tags are held in memory
//! ([`FakeTags`]) and the lyrics files and the run's state file are real.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use octo_core::lyrics::{
    ILyricsSource, LyricsLibraryMode, LyricsLibraryRequest, LyricsLibraryRow, LyricsLibraryRun,
    LyricsLibraryStatus, LyricsLookup, LyricsResult, LyricsReviewEntry,
};
use octo_core::settings::{AppSettings, MetadataSettings, SettingsStore};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::local::LocalSongMapping;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::lyrics::lyrics_service::LyricsService;
use crate::services::lyrics::test_support::{AskingSource, FakeTags};

const MARK: &str = LyricsSidecarWriter::OCTO_MARK;

struct Library {
    dir: tempfile::TempDir,
    music: PathBuf,
    tags: Arc<FakeTags>,
}

impl Library {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).expect("the music folder");
        Self {
            dir,
            music,
            tags: Arc::new(FakeTags::default()),
        }
    }

    fn state(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `Song(name, artist, title)`.
    fn song(&self, name: &str, artist: &str, title: &str) -> PathBuf {
        let path = self.music.join(name);
        std::fs::write(&path, b"ID3 not really audio").expect("written");
        self.tags.tag(&path, Some(artist), title);
        path
    }

    /// `Job(source, statePath, besideAll)`: the source alone, saved beside, the run in
    /// `state` (or in memory), no pause between songs (`job_with_gap` sets one).
    fn job(
        &self,
        source: Arc<AskingSource>,
        state: Option<PathBuf>,
        beside_all: bool,
        library: FakeLocalLibrary,
    ) -> (Arc<LyricsLibraryWorker>, Arc<LyricsLibraryStore>) {
        self.job_with_gap(source, state, beside_all, library, Duration::ZERO)
    }

    fn job_with_gap(
        &self,
        source: Arc<AskingSource>,
        state: Option<PathBuf>,
        beside_all: bool,
        library: FakeLocalLibrary,
        gap: Duration,
    ) -> (Arc<LyricsLibraryWorker>, Arc<LyricsLibraryStore>) {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            metadata: MetadataSettings {
                fetch_lyrics: true,
                lyrics_sources: source.key().to_string(),
                write_lyrics_beside_all_songs: beside_all,
                ..MetadataSettings::default()
            },
            ..AppSettings::default()
        }));
        let lyrics = Arc::new(LyricsService::new(
            vec![source as Arc<dyn ILyricsSource>],
            settings.clone(),
        ));
        let writer = Arc::new(LyricsSidecarWriter::with_tags(
            lyrics,
            settings.clone(),
            self.tags.clone(),
        ));
        let store = Arc::new(LyricsLibraryStore::new(state));
        let music = self.music.to_string_lossy().into_owned();
        let worker = LyricsLibraryWorker::new(
            store.clone(),
            writer,
            settings,
            Arc::new(library),
            move || music.clone(),
            Arc::new(LyricsUndoJournal::new(None)),
        )
        .with_gap(gap);
        (Arc::new(worker), store)
    }
}

fn words_of(title: &str) -> LyricsLookup {
    LyricsLookup::new(
        Some(LyricsResult::new(
            "KuGou",
            Some(format!("[00:01.00]<00:01.00>{title}<00:02.00>")),
            None,
            false,
        )),
        false,
    )
}

fn x() -> LyricsLookup {
    LyricsLookup::new(
        Some(LyricsResult::new(
            "KuGou",
            Some("[00:01.00]x".to_string()),
            None,
            false,
        )),
        false,
    )
}

async fn walk(worker: &LyricsLibraryWorker, request: LyricsLibraryRequest) {
    worker
        .run_request(&request, &CancellationToken::new())
        .await
        .expect("the walk runs");
}

fn lrc(path: &Path) -> PathBuf {
    path.with_extension("lrc")
}

#[tokio::test]
async fn job_stopped_part_way_resumes_where_it_was_and_writes_the_rest() {
    let library = Library::new();
    let songs: Vec<PathBuf> = (1..=5)
        .map(|n| library.song(&format!("{n:02} Song {n}.mp3"), "Artist", &format!("Song {n}")))
        .collect();
    let source = AskingSource::new("kugou", |query| words_of(&query.title));
    let state = library.state("lyrics-library.json");
    let (worker, store) = library.job(
        source.clone(),
        Some(state.clone()),
        true,
        FakeLocalLibrary::default(),
    );
    let stopper = Arc::downgrade(&worker);
    source.set_after(Some(Box::new(move |asked| {
        if asked == 2
            && let Some(worker) = stopper.upgrade()
        {
            worker.request_cancel();
        }
    })));

    walk(&worker, LyricsLibraryRequest::new(false)).await;

    assert_eq!(store.current().status, LyricsLibraryStatus::Cancelled);
    assert_eq!(store.current().cursor, 2);
    assert!(store.current().can_resume());
    // `store.Dispose()`.
    store.flush();
    drop((worker, store));

    // A restart in between: the run is read back from disk.
    let (resumed, reread) = library.job(source.clone(), Some(state), true, FakeLocalLibrary::default());
    source.set_after(None);
    walk(
        &resumed,
        LyricsLibraryRequest {
            resume: true,
            ..LyricsLibraryRequest::new(false)
        },
    )
    .await;

    assert_eq!(reread.current().status, LyricsLibraryStatus::Completed);
    assert_eq!(source.asked(), ["Song 1", "Song 2", "Song 3", "Song 4", "Song 5"]);
    assert_eq!(reread.current().written, 5);
    assert_eq!(reread.current().word_timed, 5);
    for song in &songs {
        let text = std::fs::read_to_string(lrc(song)).expect("written");
        assert!(text.starts_with(MARK), "{}", song.display());
    }
}

#[test]
fn job_a_run_going_when_octo_stopped_comes_back_interrupted_not_running() {
    let library = Library::new();
    let state = library.state("lyrics-library.json");
    let run = LyricsLibraryRun {
        status: LyricsLibraryStatus::Running,
        queue: vec!["a".into(), "b".into()],
        cursor: 1,
        total: 2,
        ..LyricsLibraryRun::default()
    };
    std::fs::write(&state, octo_core::json::to_string(&run)).expect("written");

    let store = LyricsLibraryStore::new(Some(state));

    assert_eq!(store.current().status, LyricsLibraryStatus::Interrupted);
    assert!(store.current().can_resume());
    // The reason the dashboard shows, word for word.
    assert_eq!(
        store.current().reason.as_deref(),
        Some("Octo restarted while this run was in progress.")
    );
}

#[tokio::test]
async fn job_services_stop_answering_pauses_so_a_resume_asks_again() {
    let library = Library::new();
    for n in 1..=LyricsLibraryWorker::BUSY_IN_A_ROW_LIMIT + 2 {
        library.song(&format!("{n:02}.mp3"), "Artist", &format!("Song {n}"));
    }
    let source = AskingSource::new("kugou", |_| LyricsLookup::failed());
    let (worker, store) = library.job(
        source.clone(),
        Some(library.state("state.json")),
        true,
        FakeLocalLibrary::default(),
    );

    walk(&worker, LyricsLibraryRequest::new(false)).await;

    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Interrupted);
    assert_eq!(run.cursor, 0);
    assert_eq!(run.busy, 0);
    assert_eq!(run.processed, 0);
    // Rust-only: the reason, and only the wall was asked.
    assert_eq!(
        run.reason.as_deref(),
        Some("The lyrics services stopped answering. Resume later to carry on from here.")
    );
    assert_eq!(
        source.asked().len(),
        LyricsLibraryWorker::BUSY_IN_A_ROW_LIMIT as usize
    );
}

#[tokio::test]
async fn job_uncertain_match_goes_on_the_review_list() {
    let library = Library::new();
    let path = library.song("01.mp3", "Artist", "Song");
    let source = AskingSource::new("kugou", |_| {
        let mut found = LyricsResult::new("KuGou", Some("[00:01.00]x".to_string()), None, false)
            .with_candidate_id("kugou:1.a");
        found.doubt = Some("lengths differ by 2 s".to_string());
        LyricsLookup::new(Some(found), false)
    });
    let (worker, store) = library.job(
        source,
        Some(library.state("state.json")),
        true,
        FakeLocalLibrary::default(),
    );

    walk(&worker, LyricsLibraryRequest::new(false)).await;

    let review = store.current().review;
    assert_eq!(review.len(), 1);
    let entry = &review[0];
    assert_eq!(entry.path, path.to_string_lossy());
    assert_eq!(entry.candidate_id.as_deref(), Some("kugou:1.a"));
    assert_eq!(entry.reason, "lengths differ by 2 s");
    // Rust-only: the rest of the entry.
    assert_eq!(
        (
            entry.artist.as_str(),
            entry.title.as_str(),
            entry.kind.as_str(),
            entry.source.as_str()
        ),
        ("Artist", "Song", "line", "KuGou")
    );
    worker.dismiss_review(&path.to_string_lossy());
    assert!(store.current().review.is_empty());
}

#[tokio::test]
async fn job_by_default_only_walks_octos_downloads() {
    let library = Library::new();
    let mine = library.song("01 Mine.mp3", "Artist", "Mine");
    library.song("02 Theirs.mp3", "Artist", "Theirs");
    let source = AskingSource::new("kugou", |_| x());
    let downloads = FakeLocalLibrary::with_mapping(
        "Artist",
        "Mine",
        None,
        LocalSongMapping {
            local_path: mine.to_string_lossy().into_owned(),
            ..LocalSongMapping::default()
        },
    );
    let (worker, store) = library.job(source.clone(), None, false, downloads);

    walk(&worker, LyricsLibraryRequest::new(false)).await;

    assert_eq!(store.current().scope, "OctoDownloads");
    assert_eq!(source.asked(), ["Mine"]);
    assert!(!library.music.join("02 Theirs.lrc").exists());
}

// ---- Rust-only: the store -------------------------------------------------------------------

fn fixture() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/lyrics-library.json"
    );
    std::fs::read_to_string(path).expect("the fixture is in the repo")
}

/// The store loads the fixture and writes it back unchanged (it is Completed, so the load
/// leaves it alone).
#[test]
fn the_store_writes_the_fixture_back_byte_for_byte() {
    let library = Library::new();
    let state = library.state("lyrics-library.json");
    let text = fixture();
    std::fs::write(&state, &text).expect("copied");

    let store = LyricsLibraryStore::new(Some(state.clone()));
    assert_eq!(store.current().rows.len(), 2);
    store.replace(store.current());

    assert_eq!(
        std::fs::read_to_string(&state).expect("written"),
        text.trim_end_matches('\n')
    );
    assert!(!state_file::temp_path(&state).exists());
}

/// The fixture as a run that was going: it comes back Interrupted with the C#'s reason, and
/// everything else as it was.
#[test]
fn a_running_fixture_comes_back_interrupted_with_everything_else_kept() {
    let library = Library::new();
    let state = library.state("lyrics-library.json");
    let text = fixture().replace("\"Status\":2", "\"Status\":1");
    std::fs::write(&state, &text).expect("copied");

    let store = LyricsLibraryStore::new(Some(state.clone()));
    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Interrupted);
    assert_eq!(
        run.reason.as_deref(),
        Some(octo_core::lyrics::lyrics_library_steps::OCTO_RESTARTED)
    );
    // Its cursor is at the end, so there is nothing to resume.
    assert!(!run.can_resume());
    store.replace(run);
    let expected = fixture()
        .trim_end_matches('\n')
        .replace("\"Status\":2", "\"Status\":4")
        .replace(
            "\"Reason\":null,\"Errors\"",
            "\"Reason\":\"Octo restarted while this run was in progress.\",\"Errors\"",
        );
    assert_eq!(std::fs::read_to_string(&state).expect("written"), expected);
}

#[test]
fn an_unreadable_or_null_state_file_starts_idle() {
    let library = Library::new();
    let state = library.state("lyrics-library.json");
    for text in ["{ not the shape we wrote", "null", ""] {
        std::fs::write(&state, text).expect("written");
        let store = LyricsLibraryStore::new(Some(state.clone()));
        assert_eq!(store.current(), LyricsLibraryRun::default(), "{text:?}");
    }
    assert_eq!(
        LyricsLibraryStore::new(Some(library.state("missing.json")))
            .current()
            .status,
        LyricsLibraryStatus::Idle
    );
}

/// The review keeps its last 500 entries and the errors their last 20.
#[test]
fn update_bounds_the_review_and_the_error_list() {
    let store = LyricsLibraryStore::new(None);
    store.update(|run| {
        for i in 0..LyricsLibraryStore::MAX_REVIEW + 5 {
            run.review.push(LyricsReviewEntry {
                path: format!("/music/{i}.flac"),
                ..LyricsReviewEntry::default()
            });
        }
        for i in 0..30 {
            run.errors.push(format!("error {i}"));
        }
    });
    let run = store.current();
    assert_eq!(run.review.len(), LyricsLibraryStore::MAX_REVIEW);
    assert_eq!(run.review[0].path, "/music/5.flac");
    assert_eq!(run.errors.len(), 20);
    assert_eq!(run.errors[0], "error 10");
}

/// An update is written by the flusher, not at once; a replace is written at once; cancelling
/// the flusher writes what is left.
#[tokio::test(start_paused = true)]
async fn updates_are_flushed_on_the_timer_and_at_shutdown() {
    let library = Library::new();
    let state = library.state("lyrics-library.json");
    let store = Arc::new(LyricsLibraryStore::new(Some(state.clone())));
    let stopping = CancellationToken::new();
    let flusher = tokio::spawn(store.clone().run_flusher(stopping.clone()));

    store.update(|run| run.run_id = "first".into());
    assert!(!state.exists());
    tokio::time::sleep(LyricsLibraryStore::FLUSH_INTERVAL + Duration::from_millis(10)).await;
    assert!(
        std::fs::read_to_string(&state)
            .expect("flushed")
            .contains("\"first\"")
    );

    store.replace(LyricsLibraryRun {
        run_id: "second".into(),
        ..LyricsLibraryRun::default()
    });
    assert!(
        std::fs::read_to_string(&state)
            .expect("written")
            .contains("\"second\"")
    );

    store.update(|run| run.run_id = "third".into());
    stopping.cancel();
    flusher.await.expect("joins").expect("the flusher ends");
    assert!(
        std::fs::read_to_string(&state)
            .expect("flushed")
            .contains("\"third\"")
    );
}

// ---- Rust-only: the worker ------------------------------------------------------------------

/// The hosted loop runs what the dashboard queues, one at a time; a second request while one
/// waits or runs is refused; a step whose rows cannot be read fails the run with the message.
#[tokio::test]
async fn the_hosted_loop_runs_queued_requests_and_records_a_failure() {
    let library = Library::new();
    library.song("01.mp3", "Artist", "Song");
    let source = AskingSource::new("kugou", |_| x());
    let (worker, store) = library.job(source, None, true, FakeLocalLibrary::default());
    let stopping = CancellationToken::new();
    let hosted = tokio::spawn(worker.clone().run(stopping.clone()));

    assert!(worker.try_enqueue(LyricsLibraryRequest::new(false)));
    assert!(!worker.try_enqueue(LyricsLibraryRequest::new(false)));
    assert!(worker.is_running());
    wait_until(|| !worker.is_running()).await;
    assert_eq!(store.current().status, LyricsLibraryStatus::Completed);
    assert_eq!(store.current().written, 1);
    assert_eq!(store.current().scope, "WholeLibrary");

    // Two rows with one id: the C# ToDictionary threw.
    store.update(|run| {
        let row = LyricsLibraryRow {
            id: "same".into(),
            ..LyricsLibraryRow::default()
        };
        run.rows = vec![row.clone(), row];
    });
    assert!(worker.try_enqueue(LyricsLibraryRequest {
        mode: LyricsLibraryMode::Preview,
        picked: Some(vec!["same".into()]),
        ..LyricsLibraryRequest::default()
    }));
    wait_until(|| !worker.is_running()).await;
    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Failed);
    assert_eq!(
        run.reason.as_deref(),
        Some("An item with the same key has already been added. Key: same")
    );
    assert!(run.finished_utc.is_some());

    stopping.cancel();
    hosted.await.expect("joins").expect("the loop ends");
}

/// Shutting down mid-walk leaves the run Interrupted where it was, so it can be resumed. As in
/// C#, the song whose lookup the shutdown cut into is counted (a cancelled lookup answers "not
/// now", so it is busy), and the pause after it ends the run.
#[tokio::test]
async fn a_shutdown_mid_walk_leaves_the_run_interrupted() {
    let library = Library::new();
    for n in 1..=3 {
        library.song(&format!("0{n}.mp3"), "Artist", &format!("Song {n}"));
    }
    let source = AskingSource::new("kugou", |query| words_of(&query.title));
    let (worker, store) = library.job(source.clone(), None, true, FakeLocalLibrary::default());
    let stopping = CancellationToken::new();
    let stop = stopping.clone();
    source.set_after(Some(Box::new(move |asked| {
        if asked == 2 {
            stop.cancel();
        }
    })));

    worker
        .run_request(&LyricsLibraryRequest::new(false), &stopping)
        .await
        .expect("stopped");

    // Song 2 was found before the shutdown, so it was written; the next song saw the token.
    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Interrupted);
    assert_eq!((run.cursor, run.written), (2, 2));
    assert!(run.can_resume());

    // Through the hosted loop, with the pause: the lookup cut into answers "not now", the song
    // is counted busy, and the pause throws, which the loop records as Interrupted. (A fresh
    // library: the songs above have lyrics now.)
    let library = Library::new();
    for n in 1..=3 {
        library.song(&format!("0{n}.mp3"), "Artist", &format!("Song {n}"));
    }
    let failing = AskingSource::new("kugou", |_| LyricsLookup::failed());
    let (worker, store) = library.job_with_gap(
        failing.clone(),
        None,
        true,
        FakeLocalLibrary::default(),
        LyricsLibraryWorker::GAP,
    );
    let stopping = CancellationToken::new();
    let stop = stopping.clone();
    failing.set_after(Some(Box::new(move |_| stop.cancel())));
    let hosted = tokio::spawn(worker.clone().run(stopping.clone()));
    assert!(worker.try_enqueue(LyricsLibraryRequest::new(false)));
    hosted.await.expect("joins").expect("the loop ends");
    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Interrupted);
    assert_eq!((run.cursor, run.busy, run.processed), (1, 1, 1));
    assert_eq!(failing.asked().len(), 1);
    assert!(run.can_resume());
    assert!(!worker.is_running());
}

/// Octo's downloads: only files still there, once each ignoring case, in ordinal (UTF-16)
/// order; the whole library: audio files only, in every folder.
#[tokio::test]
async fn enumerate_lists_the_songs_a_run_walks() {
    let library = Library::new();
    let nested = library.music.join("Artist").join("Album");
    std::fs::create_dir_all(&nested).expect("folders");
    for name in [
        "b.FLAC",
        "a.mp3",
        "cover.jpg",
        ".mp3",
        "notes.mp3.txt",
        "\u{FF5E}.mp3",
        "\u{1F3B5}.mp3",
    ] {
        std::fs::write(nested.join(name), b"x").expect("written");
    }
    let (worker, _) = library.job(
        AskingSource::new("kugou", |_| x()),
        None,
        true,
        FakeLocalLibrary::default(),
    );
    let whole = worker.enumerate(true).await;
    let names: Vec<String> = whole
        .iter()
        .map(|path| {
            Path::new(path)
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // U+1F3B5 is a surrogate pair in UTF-16 (D83C ...), so it sorts before U+FF5E.
    assert_eq!(
        names,
        [".mp3", "a.mp3", "b.FLAC", "\u{1F3B5}.mp3", "\u{FF5E}.mp3"]
    );

    let a = nested.join("a.mp3").to_string_lossy().into_owned();
    let b = nested.join("b.FLAC").to_string_lossy().into_owned();
    let downloads = FakeLocalLibrary::default();
    for path in [
        &b,
        &a,
        &a.to_uppercase(),
        &"/nowhere/x.mp3".to_string(),
        &String::new(),
    ] {
        downloads.by_tags.lock().push((
            String::new(),
            String::new(),
            None,
            LocalSongMapping {
                local_path: path.clone(),
                ..LocalSongMapping::default()
            },
        ));
    }
    let (worker, _) = library.job(AskingSource::new("kugou", |_| x()), None, false, downloads);
    assert_eq!(worker.enumerate(false).await, [a, b]);

    assert_eq!(extension_of("song.MP3"), ".MP3");
    assert_eq!(extension_of("song."), "");
    assert_eq!(extension_of("song"), "");
}

/// What a song is looked up by: the performer, else the album artist; the title for a query;
/// nothing without both.
#[test]
fn read_tags_takes_the_artist_title_album_and_length() {
    let library = Library::new();
    let path = library.song("01.mp3", "Artist", "Song (feat. Guest)");
    let (worker, _) = library.job(
        AskingSource::new("kugou", |_| x()),
        None,
        true,
        FakeLocalLibrary::default(),
    );
    let job = worker.read_tags(&path.to_string_lossy()).expect("tags");
    assert_eq!((job.artist.as_str(), job.title.as_str()), ("Artist", "Song"));
    assert_eq!(job.attempt, 1);

    library.tags.tag(&path, None, "Song");
    assert!(worker.read_tags(&path.to_string_lossy()).is_none());
    assert!(worker.read_tags("/nowhere.mp3").is_none());
}

async fn wait_until(done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out");
}
