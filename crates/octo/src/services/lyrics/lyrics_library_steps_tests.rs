//! LyricsLibraryStepsTests: the lyrics page's steps on real files: a scan lists songs with no
//! lyrics or weaker ones and changes nothing, a preview looks up only the picked songs, Save
//! writes only those, and Undo puts back what Save wrote. The C# tests wrote tags into an MP3
//! fixture with TagLib; here the songs' tags are held in memory ([`FakeTags`]), the seam the
//! writer and the job read them through, and the lyrics files are real.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use octo_core::lyrics::{
    ILyricsSource, LyricsLibraryMode, LyricsLibraryRequest, LyricsLibraryStatus, LyricsLookup, LyricsQuery,
    LyricsResult,
};
use octo_core::settings::{AppSettings, LyricsSaveTo, MetadataSettings, SettingsStore};
use tokio_util::sync::CancellationToken;

use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::lyrics::lyrics_library_job::{LyricsLibraryStore, LyricsLibraryWorker};
use crate::services::lyrics::lyrics_service::LyricsService;
use crate::services::lyrics::lyrics_sidecar_writer::LyricsSidecarWriter;
use crate::services::lyrics::lyrics_undo_journal::LyricsUndoJournal;
use crate::services::lyrics::test_support::{AskingSource, FakeTags};

const MARK: &str = LyricsSidecarWriter::OCTO_MARK;
const WORDS: &str = "[00:01.00]<00:01.00>word <00:01.50>by word<00:02.00>";
const LINES: &str = "[00:01.00]line by line";

struct Library {
    _dir: tempfile::TempDir,
    music: PathBuf,
    tags: Arc<FakeTags>,
}

impl Library {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).expect("the music folder");
        Self {
            _dir: dir,
            music,
            tags: Arc::new(FakeTags::default()),
        }
    }

    /// `Song(name, artist, title, tagLyrics)`: a song file with its performer, title and, when
    /// given, lyrics in its tags.
    fn song(&self, name: &str, artist: Option<&str>, title: &str, tag_lyrics: Option<&str>) -> PathBuf {
        let path = self.music.join(name);
        std::fs::write(&path, b"ID3 not really audio").expect("written");
        self.tags.tag(&path, artist, title);
        if let Some(lyrics) = tag_lyrics {
            self.tags.set(&path, lyrics);
        }
        path
    }

    /// `TagLyrics(path)`.
    fn tag_lyrics(&self, path: &Path) -> Option<String> {
        self.tags.get(path)
    }

    /// `Worker(answer, saveTo)`: KuGou after the song's own lyrics, saved where `save_to` says,
    /// with no pause between songs, the run in memory and the undo journal in memory.
    fn worker(
        &self,
        answer: impl Fn(&LyricsQuery) -> Option<LyricsResult> + Send + Sync + 'static,
        save_to: &str,
    ) -> (
        Arc<LyricsLibraryWorker>,
        Arc<LyricsLibraryStore>,
        Arc<AskingSource>,
    ) {
        let source = AskingSource::new("kugou", move |query| {
            answer(query).map_or_else(LyricsLookup::miss, |found| LyricsLookup::new(Some(found), false))
        });
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            metadata: MetadataSettings {
                fetch_lyrics: true,
                lyrics_sources: "song,kugou".to_string(),
                save_lyrics_to: save_to.to_string(),
                ..MetadataSettings::default()
            },
            ..AppSettings::default()
        }));
        let lyrics = Arc::new(LyricsService::new(
            vec![source.clone() as Arc<dyn ILyricsSource>],
            settings.clone(),
        ));
        let writer = Arc::new(LyricsSidecarWriter::with_tags(
            lyrics,
            settings.clone(),
            self.tags.clone(),
        ));
        let store = Arc::new(LyricsLibraryStore::new(None));
        let music = self.music.to_string_lossy().into_owned();
        let worker = LyricsLibraryWorker::new(
            store.clone(),
            writer,
            settings,
            Arc::new(FakeLocalLibrary::default()),
            move || music.clone(),
            Arc::new(LyricsUndoJournal::new(None)),
        )
        .with_gap(Duration::ZERO);
        (Arc::new(worker), store, source)
    }
}

fn kugou_words(_: &LyricsQuery) -> Option<LyricsResult> {
    Some(LyricsResult::new("KuGou", Some(WORDS.to_string()), None, false))
}

/// `Step(worker, mode, picked)`: one step over the whole library.
async fn step(worker: &LyricsLibraryWorker, mode: LyricsLibraryMode, picked: Option<Vec<String>>) {
    let request = LyricsLibraryRequest {
        mode,
        scope: Some("WholeLibrary".to_string()),
        picked,
        ..LyricsLibraryRequest::default()
    };
    worker
        .run_request(&request, &CancellationToken::new())
        .await
        .expect("the step runs");
}

fn lrc(path: &Path) -> PathBuf {
    path.with_extension("lrc")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("the file is there")
}

#[tokio::test]
async fn scan_lists_missing_and_weaker_lyrics_and_changes_nothing() {
    let library = Library::new();
    let none = library.song("01 None.mp3", Some("Artist"), "None", None);
    let line = library.song("02 Line.mp3", Some("Artist"), "Line", Some(LINES));
    let word = library.song("03 Word.mp3", Some("Artist"), "Word", None);
    std::fs::write(lrc(&word), format!("{MARK}\n{WORDS}\n")).expect("written");
    library.song("04 Untagged.mp3", None, "Untagged", None);
    let (worker, store, source) = library.worker(kugou_words, LyricsSaveTo::BESIDE);

    step(&worker, LyricsLibraryMode::Scan, None).await;

    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Completed);
    let rows: Vec<(&str, &str)> = run
        .rows
        .iter()
        .map(|row| (row.title.as_str(), row.has.as_str()))
        .collect();
    assert_eq!(rows, [("None", "none"), ("Line", "line")]);
    assert_eq!(run.word_already, 1);
    assert_eq!(run.skipped, 1);
    assert!(source.asked().is_empty());
    assert!(!lrc(&none).exists());
    assert_eq!(library.tag_lyrics(&line).as_deref(), Some(LINES));
}

#[tokio::test]
async fn preview_save_undo_work_only_on_the_picked_songs() {
    let library = Library::new();
    let none = library.song("01 None.mp3", Some("Artist"), "None", None);
    let line = library.song("02 Line.mp3", Some("Artist"), "Line", Some(LINES));
    let skipped = library.song("03 Skipped.mp3", Some("Artist"), "Skipped", None);
    let (worker, store, source) = library.worker(kugou_words, LyricsSaveTo::BESIDE);
    step(&worker, LyricsLibraryMode::Scan, None).await;
    let ids: HashMap<String, String> = store
        .current()
        .rows
        .iter()
        .map(|row| (row.title.clone(), row.id.clone()))
        .collect();

    step(
        &worker,
        LyricsLibraryMode::Preview,
        Some(vec![ids["None"].clone(), ids["Line"].clone()]),
    )
    .await;

    assert_eq!(source.asked(), ["None", "Line"]);
    for row in store.current().rows.iter().filter(|row| row.title != "Skipped") {
        assert_eq!(row.result, "found", "{}", row.title);
        assert_eq!(row.kind.as_deref(), Some("word"), "{}", row.title);
        assert_eq!(row.preview, ["word by word"], "{}", row.title);
    }
    assert!(!lrc(&none).exists());

    step(
        &worker,
        LyricsLibraryMode::Save,
        Some(vec![
            ids["None"].clone(),
            ids["Line"].clone(),
            ids["Skipped"].clone(),
        ]),
    )
    .await;

    assert_eq!(store.current().written, 2);
    assert!(read(&lrc(&none)).starts_with(MARK));
    // Someone else's lyrics in the tags stay, and the better ones go beside them.
    assert_eq!(library.tag_lyrics(&line).as_deref(), Some(LINES));
    assert!(read(&lrc(&line)).starts_with(MARK));
    assert!(!lrc(&skipped).exists());
    assert!(worker.can_undo());

    step(&worker, LyricsLibraryMode::Undo, None).await;

    assert!(!lrc(&none).exists());
    assert!(!lrc(&line).exists());
    assert_eq!(library.tag_lyrics(&line).as_deref(), Some(LINES));
    assert!(!worker.can_undo());
    assert_eq!(store.current().written, 2);
}

#[tokio::test]
async fn save_inside_then_undo_puts_the_tags_back() {
    let library = Library::new();
    let song = library.song("01 None.mp3", Some("Artist"), "None", None);
    let (worker, store, _) = library.worker(kugou_words, LyricsSaveTo::INSIDE);
    step(&worker, LyricsLibraryMode::Scan, None).await;
    let rows = store.current().rows;
    assert_eq!(rows.len(), 1);
    let id = rows[0].id.clone();
    step(&worker, LyricsLibraryMode::Preview, Some(vec![id.clone()])).await;

    step(&worker, LyricsLibraryMode::Save, Some(vec![id])).await;

    assert!(
        library
            .tag_lyrics(&song)
            .is_some_and(|lyrics| lyrics.starts_with(MARK))
    );
    assert!(!lrc(&song).exists());

    step(&worker, LyricsLibraryMode::Undo, None).await;

    assert!(library.tag_lyrics(&song).is_none_or(|lyrics| lyrics.is_empty()));
}

#[tokio::test]
async fn preview_nothing_better_is_not_offered_for_saving() {
    let library = Library::new();
    library.song("01 Line.mp3", Some("Artist"), "Line", Some(LINES));
    let (worker, store, _) = library.worker(
        |_| {
            Some(LyricsResult::new(
                "LRCLIB",
                Some("[00:01.00]other lines".to_string()),
                None,
                false,
            ))
        },
        LyricsSaveTo::BESIDE,
    );
    step(&worker, LyricsLibraryMode::Scan, None).await;
    let id = store.current().rows[0].id.clone();

    step(&worker, LyricsLibraryMode::Preview, Some(vec![id.clone()])).await;
    step(&worker, LyricsLibraryMode::Save, Some(vec![id])).await;

    let run = store.current();
    assert_eq!(run.rows.len(), 1);
    assert_eq!(run.rows[0].result, "none");
    assert_eq!(run.written, 0);
    assert!(!worker.can_undo());
    // Rust-only: the Save had nothing picked, and said so.
    assert_eq!(run.reason.as_deref(), Some("Nothing was picked."));
    assert_eq!(run.status, LyricsLibraryStatus::Completed);
}

#[tokio::test]
async fn undo_leaves_a_file_that_changed_since_alone() {
    let library = Library::new();
    let song = library.song("01 None.mp3", Some("Artist"), "None", None);
    let (worker, store, _) = library.worker(kugou_words, LyricsSaveTo::BESIDE);
    step(&worker, LyricsLibraryMode::Scan, None).await;
    let id = store.current().rows[0].id.clone();
    step(&worker, LyricsLibraryMode::Preview, Some(vec![id.clone()])).await;
    step(&worker, LyricsLibraryMode::Save, Some(vec![id])).await;
    std::fs::write(lrc(&song), "[00:01.00]the owner's own now\n").expect("written");

    step(&worker, LyricsLibraryMode::Undo, None).await;

    assert_eq!(read(&lrc(&song)), "[00:01.00]the owner's own now\n");
    assert_eq!(store.current().skipped, 1);
    // Rust-only: the reason, and the row is offered again.
    assert_eq!(
        store.current().reason.as_deref(),
        Some("1 changed since they were saved, so they were left alone.")
    );
}

// ---- Rust-only ------------------------------------------------------------------------------

/// A preview of a song whose file went away fails it; a song no service answers for is busy;
/// an instrumental is "none" with the kind said; Save keeps a song that has as good by then.
#[tokio::test]
async fn preview_and_save_say_what_became_of_each_song() {
    let library = Library::new();
    let gone = library.song("01 Gone.mp3", Some("Artist"), "Gone", None);
    library.song("02 Busy.mp3", Some("Artist"), "Busy", None);
    library.song("03 Instrumental.mp3", Some("Artist"), "Instrumental", None);
    let kept = library.song("04 Kept.mp3", Some("Artist"), "Kept", None);
    let source = AskingSource::new("kugou", |query| match query.title.as_str() {
        "Busy" => LyricsLookup::failed(),
        "Instrumental" => LyricsLookup::new(Some(LyricsResult::new("KuGou", None, None, true)), false),
        _ => LyricsLookup::new(
            Some(LyricsResult::new("KuGou", Some(WORDS.to_string()), None, false)),
            false,
        ),
    });
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        metadata: MetadataSettings {
            fetch_lyrics: true,
            lyrics_sources: "song,kugou".to_string(),
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
        library.tags.clone(),
    ));
    let store = Arc::new(LyricsLibraryStore::new(None));
    let music = library.music.to_string_lossy().into_owned();
    let worker = LyricsLibraryWorker::new(
        store.clone(),
        writer,
        settings,
        Arc::new(FakeLocalLibrary::default()),
        move || music.clone(),
        Arc::new(LyricsUndoJournal::new(None)),
    )
    .with_gap(Duration::ZERO);
    step(&worker, LyricsLibraryMode::Scan, None).await;
    let ids: Vec<String> = store.current().rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids.len(), 4);
    std::fs::remove_file(&gone).expect("removed");

    step(&worker, LyricsLibraryMode::Preview, Some(ids.clone())).await;

    let run = store.current();
    let results: Vec<(&str, &str, Option<&str>)> = run
        .rows
        .iter()
        .map(|row| (row.title.as_str(), row.result.as_str(), row.kind.as_deref()))
        .collect();
    assert_eq!(
        results,
        [
            ("Gone", "failed", None),
            ("Busy", "busy", None),
            ("Instrumental", "none", Some("instrumental")),
            ("Kept", "found", Some("word")),
        ]
    );
    assert_eq!((run.failed, run.busy, run.not_found, run.upgraded), (1, 1, 1, 1));
    assert_eq!(run.errors, [format!("{}: the file is gone", gone.display())]);
    assert_eq!(run.picked.as_deref(), Some(&ids[..]));

    // By the time of the Save, the song has word-timed lyrics of its own.
    std::fs::write(lrc(&kept), "[00:01.00]<00:01.00>mine<00:02.00>\n").expect("written");
    step(&worker, LyricsLibraryMode::Save, Some(ids)).await;

    let run = store.current();
    assert_eq!(run.total, 1);
    assert_eq!(run.already_had, 1);
    assert_eq!(run.rows[3].result, "kept");
    assert!(!worker.can_undo());
}

/// A stopped preview resumes where it was, with the same picks; an Undo with nothing to put
/// back says so.
#[tokio::test]
async fn a_stopped_preview_resumes_and_an_empty_undo_says_so() {
    let library = Library::new();
    for n in 1..=3 {
        library.song(&format!("0{n}.mp3"), Some("Artist"), &format!("Song {n}"), None);
    }
    let (worker, store, source) = library.worker(kugou_words, LyricsSaveTo::BESIDE);
    step(&worker, LyricsLibraryMode::Scan, None).await;
    let ids: Vec<String> = store.current().rows.iter().map(|row| row.id.clone()).collect();
    let stopper = Arc::downgrade(&worker);
    source.set_after(Some(Box::new(move |asked| {
        if asked == 1
            && let Some(worker) = stopper.upgrade()
        {
            worker.request_cancel();
        }
    })));

    step(&worker, LyricsLibraryMode::Preview, Some(ids)).await;

    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Cancelled);
    assert_eq!(run.reason.as_deref(), Some("Stopped from the dashboard."));
    assert_eq!(run.cursor, 1);
    assert!(run.can_resume());
    source.set_after(None);

    let resume = LyricsLibraryRequest {
        resume: true,
        ..LyricsLibraryRequest::default()
    };
    worker
        .run_request(&resume, &CancellationToken::new())
        .await
        .expect("resumed");

    let run = store.current();
    assert_eq!(run.status, LyricsLibraryStatus::Completed);
    assert_eq!(source.asked(), ["Song 1", "Song 2", "Song 3"]);
    assert_eq!(run.upgraded, 3);
    assert!(run.rows.iter().all(|row| row.result == "found"));

    step(&worker, LyricsLibraryMode::Undo, None).await;
    let run = store.current();
    assert_eq!(run.reason.as_deref(), Some("There was nothing to put back."));
    assert_eq!(run.total, 0);
    // The list survives every step.
    assert_eq!(run.rows.len(), 3);
}
