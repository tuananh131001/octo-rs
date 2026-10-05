//! CoverUpgradeTests, and Rust-only tests of the state files, the journal and the Navidrome
//! side.
//!
//! "Upgrade cover art" rewrites the owner's files, so what it touches, what it leaves and what
//! Undo puts back are pinned here, on real MP3s (made by ffmpeg) with real tags.

use std::io::{Cursor, Write};
use std::sync::{OnceLock, Weak};

use async_trait::async_trait;
use octo_core::settings::{AppSettings, SubsonicSettings};
use octo_media::tags::TagFields;
use octo_media::tags::tag_writer_extras;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::cover_art::album_cover_finder::FoundCover;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::test_audio::audio_or_skip;

const BACK_COVER: u8 = 4;

fn encode(image: &image::RgbImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .expect("a JPEG encodes");
    out.into_inner()
}

/// `Jpeg(side, shade)`: a flat grey square.
fn jpeg(side: u32, shade: u8) -> Vec<u8> {
    encode(&image::RgbImage::from_pixel(side, side, image::Rgb([shade; 3])))
}

/// `Stripes(side, across)`: a left-to-right ramp, or eight bars.
fn stripes(side: u32, across: bool) -> Vec<u8> {
    encode(&image::RgbImage::from_fn(side, side, |x, _| {
        let v = if across {
            (255 * x / side) as u8
        } else {
            (255 * ((x / (side / 8)) % 2)) as u8
        };
        image::Rgb([v; 3])
    }))
}

/// `FixedFinder`: always finds the same cover, and remembers what it was asked.
struct FixedFinder {
    cover: Option<FoundCover>,
    asked: Mutex<Vec<AlbumCoverQuery>>,
}

impl FixedFinder {
    fn new(cover: Option<FoundCover>) -> Arc<Self> {
        Arc::new(Self {
            cover,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn albums(&self) -> Vec<String> {
        self.asked
            .lock()
            .iter()
            .map(|query| query.album.clone().unwrap_or_default())
            .collect()
    }
}

#[async_trait]
impl IAlbumCoverFinder for FixedFinder {
    async fn find(&self, query: &AlbumCoverQuery) -> Option<FoundCover> {
        self.asked.lock().push(query.clone());
        self.cover.clone()
    }
}

fn found(side: u32, source: &str) -> Option<FoundCover> {
    Some(FoundCover::new(jpeg(side, 200), source, side as i32))
}

struct Library {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    mp3: Vec<u8>,
}

impl Library {
    fn new(mp3: Vec<u8>) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("music")).expect("created");
        std::fs::create_dir_all(root.join("config")).expect("created");
        Self {
            config: root.join("config"),
            root,
            _dir: dir,
            mp3,
        }
    }

    fn music(&self) -> PathBuf {
        self.root.join("music")
    }

    /// `Song(album, title, front, back, flat)`: a Daft Punk MP3 with these pictures.
    fn song(
        &self,
        album: &str,
        title: &str,
        front: Option<&[u8]>,
        back: Option<&[u8]>,
        flat: bool,
    ) -> String {
        let folder = if flat {
            self.music()
        } else {
            self.music().join("Daft Punk").join(album)
        };
        std::fs::create_dir_all(&folder).expect("created");
        let path = folder.join(format!("{title}.mp3"));
        std::fs::write(&path, &self.mp3).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_performers(&["Daft Punk".to_string()]);
        file.set_album_artists(&["Daft Punk".to_string()]);
        file.set_album(Some(album));
        file.set_title(Some(title));
        let mut pictures = Vec::new();
        if let Some(front) = front {
            pictures.push(TagPicture {
                picture_type: FRONT_COVER,
                mime_type: "image/jpeg".into(),
                description: String::new(),
                data: front.to_vec(),
            });
        }
        if let Some(back) = back {
            pictures.push(TagPicture {
                picture_type: BACK_COVER,
                mime_type: "image/jpeg".into(),
                description: String::new(),
                data: back.to_vec(),
            });
        }
        file.set_pictures(&pictures);
        file.save().expect("saved");
        path.to_string_lossy().into_owned()
    }

    fn worker(
        &self,
        finder: Arc<dyn IAlbumCoverFinder>,
        full_size: bool,
    ) -> (Arc<CoverUpgradeWorker>, Arc<CoverUpgradeStore>) {
        self.worker_with(
            finder,
            full_size,
            CoverUpgradeServices::default(),
            AppSettings::default(),
        )
    }

    fn worker_with(
        &self,
        finder: Arc<dyn IAlbumCoverFinder>,
        full_size: bool,
        services: CoverUpgradeServices,
        mut settings: AppSettings,
    ) -> (Arc<CoverUpgradeWorker>, Arc<CoverUpgradeStore>) {
        let store = Arc::new(CoverUpgradeStore::new(None));
        let journal = Arc::new(CoverUpgradeJournal::new(Some(
            self.config.join("cover-upgrade-journal.jsonl"),
        )));
        settings.metadata.embed_full_size_covers = full_size;
        let settings = SettingsStore::for_tests(settings);
        settings.set_raw("Library:DownloadPath", Some(&self.music().to_string_lossy()));
        let worker = Arc::new(CoverUpgradeWorker::new(
            store.clone(),
            journal,
            finder,
            Arc::new(settings),
            services,
        ));
        (worker, store)
    }
}

fn pictures(path: &str) -> Vec<TagPicture> {
    TagFile::open(path).expect("opens").pictures()
}

fn front_side(path: &str) -> u32 {
    pictures(path)
        .iter()
        .find(|picture| picture.picture_type == FRONT_COVER)
        .and_then(|picture| cover_image::measure(&picture.data))
        .map_or(0, |(width, _)| width)
}

/// `Run`: starts the worker, hands it the request, waits until it is done, stops the worker.
async fn run(worker: &Arc<CoverUpgradeWorker>, store: &CoverUpgradeStore, request: CoverUpgradeRequest) {
    let stopping = CancellationToken::new();
    let task = tokio::spawn(worker.clone().run(stopping.clone()));
    assert!(worker.try_enqueue(request), "the request was taken");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline
        && (worker.is_busy() || store.read(|run| run.status == CoverUpgradeStatus::Running))
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(store.read(|run| run.status), CoverUpgradeStatus::Running);
    stopping.cancel();
    task.await
        .expect("the worker ends")
        .expect("the worker ends cleanly");
}

fn whole(mode: CoverUpgradeMode) -> CoverUpgradeRequest {
    CoverUpgradeRequest::new(CoverUpgradeScope::WholeLibrary, mode, true)
}

#[tokio::test]
async fn a_preview_looks_each_album_up_once_and_writes_nothing() {
    let library = Library::new(audio_or_skip!("mp3"));
    let small = jpeg(200, 10);
    let one = library.song("Discovery", "One More Time", Some(&small), None, false);
    let two = library.song("Discovery", "Aerodynamic", Some(&small), None, false);
    let before = std::fs::read(&one).expect("read");
    let finder = FixedFinder::new(found(3000, "iTunes"));
    let (worker, store) = library.worker(finder.clone(), false);

    run(&worker, &store, whole(CoverUpgradeMode::Preview)).await;

    let current = store.current();
    assert_eq!(current.status, CoverUpgradeStatus::Completed);
    assert_eq!(current.upgraded, 1);
    assert_eq!(current.files, 2);
    assert_eq!(current.preview.len(), 1);
    let change = &current.preview[0];
    assert_eq!(
        (change.from_side, change.to_side, change.source.as_deref()),
        (200, 3000, Some("iTunes"))
    );
    assert_eq!(before, std::fs::read(&one).expect("read"));
    assert_eq!(front_side(&two), 200);
    let asked = finder.asked.lock().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (asked[0].artist.as_str(), asked[0].album.as_deref()),
        ("Daft Punk", Some("Discovery"))
    );
    assert!(!worker.can_undo());
}

#[tokio::test]
async fn an_upgrade_embeds_at_1500_keeps_the_back_cover_and_undo_puts_the_old_one_back() {
    let library = Library::new(audio_or_skip!("mp3"));
    let small = jpeg(200, 10);
    let back = jpeg(300, 90);
    let path = library.song("Discovery", "One More Time", Some(&small), Some(&back), false);
    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);

    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;

    assert_eq!(front_side(&path), 1500);
    let backs: Vec<TagPicture> = pictures(&path)
        .into_iter()
        .filter(|picture| picture.picture_type == BACK_COVER)
        .collect();
    assert_eq!(backs.len(), 1);
    assert_eq!(backs[0].data, back);
    assert!(worker.can_undo());

    run(&worker, &store, CoverUpgradeRequest::undo()).await;

    assert_eq!(store.current().status, CoverUpgradeStatus::Completed);
    let after = pictures(&path);
    let fronts: Vec<&TagPicture> = after.iter().filter(|p| p.picture_type == FRONT_COVER).collect();
    assert_eq!(fronts.len(), 1);
    assert_eq!(fronts[0].data, small);
    assert_eq!(after.iter().filter(|p| p.picture_type == BACK_COVER).count(), 1);
    assert!(!worker.can_undo());
    assert_eq!(
        std::fs::read_dir(library.config.join("cover-backups"))
            .expect("the backups folder")
            .count(),
        0
    );
}

#[tokio::test]
async fn full_size_embeds_the_master_as_found() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song("Discovery", "One More Time", Some(&jpeg(200, 10)), None, false);
    let (worker, store) = library.worker(FixedFinder::new(found(2400, "iTunes")), true);

    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;

    assert_eq!(front_side(&path), 2400);
}

#[tokio::test]
async fn a_cover_already_as_sharp_is_left_and_a_song_with_none_gets_one() {
    let library = Library::new(audio_or_skip!("mp3"));
    let sharp = library.song("Homework", "Da Funk", Some(&jpeg(1000, 10)), None, false);
    let bare = library.song("Alive 1997", "Rollin and Scratchin", None, None, false);
    let sharp_before = std::fs::read(&sharp).expect("read");
    let (worker, store) = library.worker(FixedFinder::new(found(1100, "Deezer")), false);

    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;

    assert_eq!(sharp_before, std::fs::read(&sharp).expect("read"));
    assert_eq!(front_side(&bare), 1100);
    assert_eq!((store.current().upgraded, store.current().kept), (1, 1));
}

#[tokio::test]
async fn a_soft_folder_jpeg_is_upgraded_only_when_asked() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song("Discovery", "One More Time", Some(&jpeg(1200, 10)), None, false);
    let folder_file = Path::new(&path).parent().expect("a folder").join("folder.jpg");
    let soft = jpeg(300, 50);
    std::fs::write(&folder_file, &soft).expect("written");

    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);
    run(
        &worker,
        &store,
        CoverUpgradeRequest::new(CoverUpgradeScope::WholeLibrary, CoverUpgradeMode::Apply, false),
    )
    .await;
    assert_eq!(std::fs::read(&folder_file).expect("read"), soft);

    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);
    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;
    assert_eq!(
        cover_image::measure(&std::fs::read(&folder_file).expect("read")),
        Some((3000, 3000))
    );
    let current = store.current();
    assert_eq!(current.preview.len(), 1);
    assert!(current.preview[0].folder_cover);

    run(&worker, &store, CoverUpgradeRequest::undo()).await;
    assert_eq!(std::fs::read(&folder_file).expect("read"), soft);
}

#[tokio::test]
async fn a_scan_lists_only_soft_albums_and_looks_nothing_up() {
    let library = Library::new(audio_or_skip!("mp3"));
    library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    library.song("Homework", "Da Funk", Some(&jpeg(1200, 10)), None, false);
    library.song("Alive 1997", "Rollin and Scratchin", None, None, false);
    let finder = FixedFinder::new(found(3000, "iTunes"));
    let (worker, store) = library.worker(finder.clone(), false);

    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;

    let current = store.current();
    assert_eq!(current.status, CoverUpgradeStatus::Completed);
    assert!(finder.asked.lock().is_empty());
    assert_eq!((current.soft, current.kept), (2, 1));
    let mut albums: Vec<String> = current
        .preview
        .iter()
        .filter_map(|row| row.album.clone())
        .collect();
    albums.sort();
    assert_eq!(albums, ["Alive 1997", "Discovery"]);
    assert!(current.preview.iter().all(|row| row.result == "soft"));
    let row = |album: &str| {
        current
            .preview
            .iter()
            .find(|row| row.album.as_deref() == Some(album))
            .expect("listed")
            .clone()
    };
    assert_eq!(row("Alive 1997").from_side, 0);
    assert!(worker.thumbnail(&row("Discovery").id).is_some());
    assert!(worker.thumbnail(&row("Alive 1997").id).is_none());
}

#[tokio::test]
async fn only_the_picked_albums_are_looked_up_and_upgraded() {
    let library = Library::new(audio_or_skip!("mp3"));
    let discovery = library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    let homework = library.song("Homework", "Da Funk", Some(&jpeg(300, 10)), None, false);
    let homework_before = std::fs::read(&homework).expect("read");
    let finder = FixedFinder::new(found(3000, "iTunes"));
    let (worker, store) = library.worker(finder.clone(), false);
    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;
    let pick = store.read(|run| {
        run.preview
            .iter()
            .find(|row| row.album.as_deref() == Some("Discovery"))
            .expect("listed")
            .id
            .clone()
    });

    run(
        &worker,
        &store,
        whole(CoverUpgradeMode::Apply).with_albums(vec![pick.clone()]),
    )
    .await;

    assert_eq!(front_side(&discovery), 1500);
    assert_eq!(homework_before, std::fs::read(&homework).expect("read"));
    assert_eq!(finder.albums(), ["Discovery"]);
    let current = store.current();
    assert_eq!(current.preview.len(), 1);
    let row = &current.preview[0];
    assert_eq!(
        (row.id.as_str(), row.result.as_str(), row.from_side, row.to_side),
        (pick.as_str(), "upgraded", 300, 3000)
    );
}

#[tokio::test]
async fn a_picked_album_with_nothing_larger_stays_on_the_list_as_none() {
    let library = Library::new(audio_or_skip!("mp3"));
    library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    let (worker, store) = library.worker(FixedFinder::new(None), false);
    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;
    let pick = store.read(|run| run.preview[0].id.clone());

    run(
        &worker,
        &store,
        whole(CoverUpgradeMode::Preview).with_albums(vec![pick]),
    )
    .await;

    let current = store.current();
    assert_eq!(current.preview.len(), 1);
    assert_eq!(
        (current.preview[0].result.as_str(), current.preview[0].files),
        ("none", 0)
    );
    assert_eq!(current.kept, 1);
}

#[tokio::test]
async fn a_preview_keeps_a_small_copy_of_each_cover_it_found_and_a_new_scan_forgets_them() {
    let library = Library::new(audio_or_skip!("mp3"));
    library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);
    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;
    let id = store.read(|run| run.preview[0].id.clone());
    assert!(worker.found_thumbnail(&id).is_none());

    run(
        &worker,
        &store,
        whole(CoverUpgradeMode::Preview).with_albums(vec![id.clone()]),
    )
    .await;
    let kept = worker.found_thumbnail(&id).expect("a found cover");
    assert_eq!(
        cover_image::measure(&kept),
        Some((CoverUpgradeWorker::THUMB_SIDE, CoverUpgradeWorker::THUMB_SIDE))
    );

    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;
    assert!(worker.found_thumbnail(&id).is_none());
}

/// Brandon's library is one folder of 2,400 songs: a pick must not read them all again.
#[tokio::test]
async fn in_a_flat_library_a_pick_reads_only_the_picked_albums_songs() {
    let library = Library::new(audio_or_skip!("mp3"));
    let cover = jpeg(300, 10);
    library.song("Discovery", "One More Time", Some(&cover), None, true);
    library.song("Discovery", "Aerodynamic", Some(&cover), None, true);
    library.song("Homework", "Da Funk", Some(&cover), None, true);
    library.song("Homework", "Around the World", Some(&cover), None, true);
    library.song("Homework", "Revolution 909", Some(&cover), None, true);
    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);

    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;
    let current = store.current();
    assert_eq!((current.songs_total, current.songs_read), (5, 5));
    assert_eq!(current.total, 1);
    let pick = current
        .preview
        .iter()
        .find(|row| row.album.as_deref() == Some("Discovery"))
        .expect("listed")
        .clone();
    assert_eq!(pick.paths.as_ref().map(Vec::len), Some(2));

    run(
        &worker,
        &store,
        whole(CoverUpgradeMode::Preview).with_albums(vec![pick.id]),
    )
    .await;

    let current = store.current();
    assert_eq!((current.songs_total, current.songs_read), (2, 2));
    assert_eq!(current.preview.len(), 1);
    assert_eq!(current.preview[0].result, "found");
}

#[test]
fn files_navidrome_named_an_album_for_are_one_item_per_album_and_the_rest_go_by_folder() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path().join("music");
    let at = |name: &str| root.join(name).to_string_lossy().into_owned();
    let files = [
        at("A - 1.flac"),
        at("B - 1.flac"),
        at("A - 2.flac"),
        root.join("x").join("C - 1.mp3").to_string_lossy().into_owned(),
        at("D - 1.mp3"),
    ];
    let albums = HashMap::from([
        (full_path(&at("A - 1.flac")), "alb-a".to_string()),
        (full_path(&at("A - 2.flac")), "alb-a".to_string()),
        (full_path(&at("B - 1.flac")), "alb-b".to_string()),
    ]);

    let items = CoverUpgradeWorker::by_album(&files, &albums);

    let named = |album: &str| -> Vec<&CoverUpgradeItem> {
        items
            .iter()
            .filter(|item| item.navidrome_album_id.as_deref() == Some(album))
            .collect()
    };
    assert_eq!(named("alb-a").len(), 1);
    assert_eq!(
        named("alb-a")[0].files,
        Some(vec![at("A - 1.flac"), at("A - 2.flac")])
    );
    assert_eq!(named("alb-b").len(), 1);
    let loose: Vec<&CoverUpgradeItem> = items
        .iter()
        .filter(|item| item.navidrome_album_id.is_none())
        .collect();
    assert_eq!(loose.len(), 2);
    assert!(
        loose
            .iter()
            .any(|item| item.folder == root.join("x").to_string_lossy())
    );
    assert!(loose.iter().any(|item| item.files == Some(vec![at("D - 1.mp3")])));
}

/// `StopOnFirstFinder`: the dashboard's stop arrives during the first lookup.
struct StopOnFirstFinder {
    worker: OnceLock<Weak<CoverUpgradeWorker>>,
    calls: Mutex<usize>,
}

#[async_trait]
impl IAlbumCoverFinder for StopOnFirstFinder {
    async fn find(&self, _query: &AlbumCoverQuery) -> Option<FoundCover> {
        let first = {
            let mut calls = self.calls.lock();
            *calls += 1;
            *calls == 1
        };
        if first && let Some(worker) = self.worker.get().and_then(Weak::upgrade) {
            worker.request_cancel();
        }
        found(3000, "iTunes")
    }
}

/// A stop in the middle of a folder used to mark the folder done, so a resume skipped the rest
/// of it: in a flat library, the whole library.
#[tokio::test]
async fn a_stopped_run_does_the_unfinished_part_again_on_resume_and_lists_each_album_once() {
    let library = Library::new(audio_or_skip!("mp3"));
    library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    library.song("Homework", "Da Funk", Some(&jpeg(300, 10)), None, false);
    let finder = Arc::new(StopOnFirstFinder {
        worker: OnceLock::new(),
        calls: Mutex::new(0),
    });
    let (worker, store) = library.worker(finder.clone(), false);
    finder.worker.set(Arc::downgrade(&worker)).expect("set once");
    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;
    let ids: Vec<String> = store.read(|run| run.preview.iter().map(|row| row.id.clone()).collect());
    let preview = whole(CoverUpgradeMode::Preview).with_albums(ids);

    run(&worker, &store, preview.clone()).await;
    assert_eq!(store.current().status, CoverUpgradeStatus::Cancelled);
    assert_eq!(store.current().cursor, 0);

    run(&worker, &store, preview).await;

    let current = store.current();
    assert_eq!(current.status, CoverUpgradeStatus::Completed);
    // Albums are looked up four at a time, so the stopped batch may hold both; either way it
    // is done again on resume, and each album is still listed and counted once.
    assert!(
        (3..=4).contains(&*finder.calls.lock()),
        "{} lookups",
        finder.calls.lock()
    );
    assert_eq!(current.upgraded, 2);
    assert_eq!(current.preview.len(), 2);
}

#[tokio::test]
async fn a_songs_own_barcode_tag_is_read() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    {
        let mut file = TagFile::open(&path).expect("opens");
        tag_writer_extras::set_text(&mut file, TagFields::BARCODE, Some("0724384960650"));
        file.save().expect("saved");
    }

    let read = TagFile::open(&path).expect("opens");
    assert_eq!(
        CoverUpgradeWorker::barcode_of(&read).as_deref(),
        Some("0724384960650")
    );
    assert_eq!(
        super::super::itunes_cover_art_lookup::barcode_forms(Some("0724384960650")),
        ["0724384960650", "724384960650"]
    );
    assert!(super::super::itunes_cover_art_lookup::barcode_forms(Some("not a barcode")).is_empty());
}

/// A name match can find another edition or another album: it is listed as different, and an
/// upgrade nobody picked it for leaves the song alone.
#[tokio::test]
async fn a_found_cover_that_looks_different_is_flagged_and_not_written_unpicked() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song(
        "Discovery",
        "One More Time",
        Some(&stripes(300, false)),
        None,
        false,
    );
    let before = std::fs::read(&path).expect("read");
    let different = Some(FoundCover::new(stripes(3000, true), "iTunes", 3000));
    let (worker, store) = library.worker(FixedFinder::new(different), false);

    run(&worker, &store, whole(CoverUpgradeMode::Preview)).await;
    let current = store.current();
    assert_eq!(current.preview.len(), 1);
    assert_eq!(current.preview[0].looks_same, Some(false));

    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;
    assert_eq!(before, std::fs::read(&path).expect("read"));
    assert_eq!(store.current().upgraded, 0);
}

#[tokio::test]
async fn the_same_artwork_sharper_is_marked_alike() {
    let library = Library::new(audio_or_skip!("mp3"));
    library.song(
        "Discovery",
        "One More Time",
        Some(&stripes(300, false)),
        None,
        false,
    );
    let same = Some(FoundCover::new(stripes(3000, false), "iTunes", 3000));
    let (worker, store) = library.worker(FixedFinder::new(same), false);

    run(&worker, &store, whole(CoverUpgradeMode::Preview)).await;

    let current = store.current();
    assert_eq!(current.preview.len(), 1);
    assert_eq!(current.preview[0].looks_same, Some(true));
}

// ---- Rust-only: the state files ------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state")
        .join(name)
}

#[test]
fn cover_upgrade_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(fixture("cover-upgrade.json")).expect("the fixture is there");
    let run: CoverUpgradeRun = serde_json::from_str(&text).expect("the fixture reads");
    assert_eq!(run.status, CoverUpgradeStatus::Completed);
    assert_eq!(run.mode, CoverUpgradeMode::Preview);
    assert_eq!(run.preview[0].artist, "Mötley Crüe");
    assert_eq!(run.preview[1].looks_same, None);
    assert_eq!(run.queue[0].files, None);
    assert_eq!(octo_core::json::to_string(&run), text);
}

/// The store loads the fixture (Completed, so the load leaves it alone) and writes it back
/// unchanged.
#[test]
fn the_store_writes_the_fixture_back_unchanged() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("cover-upgrade.json");
    let text = std::fs::read_to_string(fixture("cover-upgrade.json")).expect("the fixture is there");
    std::fs::write(&path, &text).expect("copied");
    let store = CoverUpgradeStore::new(Some(path.clone()));
    store.replace(store.current());
    assert_eq!(std::fs::read_to_string(&path).expect("written"), text);
    assert!(!dir.path().join("cover-upgrade.json.tmp").exists());
}

/// Missing fields take the C# initializers; DryRun and CanResume are never written.
#[test]
fn missing_fields_take_the_csharp_defaults() {
    let run: CoverUpgradeRun = serde_json::from_str(r#"{"RunId":"r","Errors":null}"#).expect("reads");
    assert!(run.folder_covers);
    assert_eq!(run.smaller_than, 1000);
    assert_eq!(run.mode, CoverUpgradeMode::Scan);
    assert!(run.errors.is_empty());
    let written = octo_core::json::to_string(&run);
    assert!(
        !written.contains("DryRun") && !written.contains("CanResume"),
        "{written}"
    );
}

#[test]
fn a_running_run_is_interrupted_on_load_and_never_resumes_itself() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("cover-upgrade.json");
    {
        let store = CoverUpgradeStore::new(Some(path.clone()));
        store.replace(CoverUpgradeRun {
            run_id: "r".into(),
            status: CoverUpgradeStatus::Running,
            cursor: 1,
            queue: vec![
                CoverUpgradeItem::new("/a", None, None),
                CoverUpgradeItem::new("/b", None, None),
            ],
            ..CoverUpgradeRun::default()
        });
    }
    let store = CoverUpgradeStore::new(Some(path));
    let run = store.current();
    assert_eq!(run.status, CoverUpgradeStatus::Interrupted);
    assert_eq!(
        run.reason.as_deref(),
        Some("Octo restarted while this run was going.")
    );
    assert!(run.can_resume());
    // An undo is never resumed.
    store.update(|run| run.undo = true);
    assert!(!store.current().can_resume());
}

#[test]
fn an_unreadable_state_file_is_an_idle_run() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("cover-upgrade.json");
    std::fs::write(&path, "{ not the shape we wrote").expect("written");
    assert_eq!(
        CoverUpgradeStore::new(Some(path)).current().status,
        CoverUpgradeStatus::Idle
    );
}

#[test]
fn update_bounds_the_list_and_the_errors() {
    let store = CoverUpgradeStore::new(None);
    store.update(|run| {
        for i in 0..CoverUpgradeStore::MAX_PREVIEW_ROWS + 5 {
            run.preview.push(CoverUpgradeChange {
                id: i.to_string(),
                ..CoverUpgradeChange::default()
            });
        }
        for i in 0..30 {
            run.errors.push(format!("error {i}"));
        }
    });
    let run = store.current();
    assert_eq!(run.preview.len(), CoverUpgradeStore::MAX_PREVIEW_ROWS);
    assert_eq!(run.errors.len(), 20);
    assert_eq!(run.errors.last().map(String::as_str), Some("error 29"));
}

/// Found covers are kept beside the run, in `cover-upgrade-found/<id>.jpg`, and cleared as a
/// folder.
#[test]
fn found_covers_are_kept_beside_the_run() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = CoverUpgradeStore::new(Some(dir.path().join("cover-upgrade.json")));
    store.save_found_thumb("abc", b"jpeg".to_vec());
    assert_eq!(
        std::fs::read(dir.path().join("cover-upgrade-found").join("abc.jpg")).expect("kept"),
        b"jpeg"
    );
    assert_eq!(store.found_thumb("abc").as_deref(), Some(&b"jpeg"[..]));
    store.clear_found_thumbs();
    assert!(!dir.path().join("cover-upgrade-found").exists());
    assert_eq!(store.found_thumb("abc"), None);
}

#[test]
fn cover_upgrade_journal_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(fixture("cover-upgrade-journal.jsonl")).expect("the fixture is there");
    for line in text.lines() {
        let entry: CoverUpgradeJournalEntry = serde_json::from_str(line).expect("a line reads");
        assert_eq!(octo_core::json::to_string(&entry), line);
    }

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("cover-upgrade-journal.jsonl");
    std::fs::write(&path, &text).expect("copied");
    let journal = CoverUpgradeJournal::new(Some(path.clone()));
    let entries = journal.read_all();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, CoverUpgradeJournal::FOLDER_FILE);
    assert_eq!(entries[0].hash, None);
    let oldest_first: Vec<CoverUpgradeJournalEntry> = entries.into_iter().rev().collect();
    journal.rewrite(&oldest_first).expect("rewritten");
    assert_eq!(std::fs::read_to_string(&path).expect("written"), text);
}

/// Each old picture is kept once, under the first 32 hex digits of its SHA-256; a line is
/// written per file; torn and pathless lines are skipped; and a rewrite drops the pictures no
/// line needs any more.
#[test]
fn the_journal_keeps_each_picture_once_and_forgets_unneeded_ones() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("cover-upgrade-journal.jsonl");
    let journal = CoverUpgradeJournal::new(Some(path.clone()));
    assert!(!journal.has_entries());

    assert!(journal.record("/m/a.mp3", CoverUpgradeJournal::EMBEDDED, Some(b"hello"), "r1"));
    assert!(journal.record("/m/b.mp3", CoverUpgradeJournal::EMBEDDED, Some(b"hello"), "r1"));
    assert!(journal.record("/m/c.mp3", CoverUpgradeJournal::EMBEDDED, None, "r1"));
    assert!(journal.record(
        "/m/cover.jpg",
        CoverUpgradeJournal::FOLDER_FILE,
        Some(b"other"),
        "r1"
    ));
    assert!(journal.has_entries());
    // The hash .NET gives "hello" (SHA-256, the first 32 hex digits, lower case).
    let hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e";
    assert_eq!(journal.backup(Some(hello)).as_deref(), Some(&b"hello"[..]));
    let backups = dir.path().join("cover-backups");
    assert_eq!(std::fs::read_dir(&backups).expect("kept").count(), 2);
    let lines = std::fs::read_to_string(&path).expect("written");
    assert!(
        lines.starts_with(&format!(
            r#"{{"p":"/m/a.mp3","k":"embedded","h":"{hello}","r":"r1"}}"#
        )),
        "{lines}"
    );
    assert!(lines.contains(r#"{"p":"/m/c.mp3","k":"embedded","h":null,"r":"r1"}"#));

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opens");
    file.write_all(b"{\"p\":\"\",\"k\":\"file\"}\n{\"p\":\"/m/torn")
        .expect("appended");
    let entries = journal.read_all();
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, ["/m/cover.jpg", "/m/c.mp3", "/m/b.mp3", "/m/a.mp3"]);

    // Keep only a.mp3: the "other" picture goes, "hello" stays.
    journal.rewrite(&[entries[3].clone()]).expect("rewritten");
    assert_eq!(std::fs::read_dir(&backups).expect("kept").count(), 1);
    assert!(journal.backup(Some(hello)).is_some());
    journal.rewrite(&[]).expect("rewritten");
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(&backups).expect("kept").count(), 0);
}

#[test]
fn album_ids_are_the_csharp_ones() {
    // CoverUpgradeWorker.AlbumId on .NET 9.
    assert_eq!(
        CoverUpgradeWorker::album_id("/music/Daft Punk/Discovery", "daft punk|discovery"),
        "c45d1ab947dbef1b"
    );
}

#[test]
fn full_path_collapses_like_get_full_path() {
    assert_eq!(full_path("/music/./a/../b//c.flac"), "/music/b/c.flac");
}

/// cover.jpg that Octo wrote keeps Octo's mark when upgraded, so a later sharper cover may
/// replace it again; the owner's own cover.jpg gets none.
#[tokio::test]
async fn octos_own_cover_jpg_keeps_its_mark() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song("Discovery", "One More Time", Some(&jpeg(1200, 10)), None, false);
    let cover = Path::new(&path).parent().expect("a folder").join("cover.jpg");
    std::fs::write(&cover, cover_image::mark_as_octo(&jpeg(300, 50))).expect("written");
    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);

    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;

    let written = std::fs::read(&cover).expect("read");
    assert!(cover_image::is_octo_cover(&written));
    assert_eq!(cover_image::measure(&written), Some((3000, 3000)));
    assert!(!Path::new(&format!("{}.octo-tmp", cover.display())).exists());
}

/// A PNG cover beside the songs is reported and left; it does not count as the album's cover.
#[tokio::test]
async fn a_png_folder_cover_is_reported_and_left() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    let png = Path::new(&path).parent().expect("a folder").join("cover.png");
    std::fs::write(&png, b"not really a png").expect("written");
    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);

    run(&worker, &store, whole(CoverUpgradeMode::Apply)).await;

    assert_eq!(std::fs::read(&png).expect("read"), b"not really a png");
    let current = store.current();
    assert_eq!(
        current.errors,
        [format!("{}: not a JPEG, left as it is", png.display())]
    );
    assert_eq!(front_side(&path), 1500);
}

/// Octo's downloads come from the mappings; without a library service the run fails with the
/// container's message, as the C# did. A real upgrade asks for a scan afterwards.
#[tokio::test]
async fn octo_downloads_walk_the_mappings_and_a_write_asks_for_a_scan() {
    let library = Library::new(audio_or_skip!("mp3"));
    let path = library.song("Discovery", "One More Time", Some(&jpeg(300, 10)), None, false);
    let (worker, store) = library.worker(FixedFinder::new(found(3000, "iTunes")), false);
    run(
        &worker,
        &store,
        CoverUpgradeRequest::new(CoverUpgradeScope::OctoDownloads, CoverUpgradeMode::Apply, true),
    )
    .await;
    assert_eq!(store.current().status, CoverUpgradeStatus::Failed);
    assert_eq!(store.current().reason.as_deref(), Some(NO_LIBRARY_SERVICE));

    let fake = Arc::new(FakeLocalLibrary::default());
    fake.by_tags.lock().push((
        "Daft Punk".into(),
        "One More Time".into(),
        None,
        crate::services::local::LocalSongMapping {
            local_path: path.clone(),
            ..Default::default()
        },
    ));
    let services = CoverUpgradeServices {
        library: Some(fake.clone()),
        ..CoverUpgradeServices::default()
    };
    let (worker, store) = library.worker_with(
        FixedFinder::new(found(3000, "iTunes")),
        false,
        services,
        AppSettings::default(),
    );
    run(
        &worker,
        &store,
        CoverUpgradeRequest::new(CoverUpgradeScope::OctoDownloads, CoverUpgradeMode::Apply, true),
    )
    .await;
    assert_eq!(store.current().status, CoverUpgradeStatus::Completed);
    assert_eq!(front_side(&path), 1500);
    assert_eq!(fake.scans(), 1);
}

// ---- Rust-only: Navidrome ------------------------------------------------------------------

fn navidrome(server: &MockServer) -> (AppSettings, Arc<NavidromeIdentityService>) {
    let settings = AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            auto_detect_download_path: false,
            ..SubsonicSettings::default()
        },
        ..AppSettings::default()
    };
    let identity = Arc::new(NavidromeIdentityService::new(
        Arc::new(SettingsStore::for_tests(settings.clone())),
        reqwest::Client::new(),
    ));
    identity.capture_login(
        br#"{"token":"jwt-1","isAdmin":true,"username":"admin","subsonicToken":"tok","subsonicSalt":"salt"}"#,
    );
    (settings, identity)
}

/// A scan asks Navidrome which album each song is, goes album by album, and reads one song of
/// each; the dashboard's tile is Navidrome's own cover of that album.
#[tokio::test]
async fn navidrome_names_the_albums_and_draws_their_tiles() {
    let mp3 = audio_or_skip!("mp3");
    let server = MockServer::start().await;
    let library = Library::new(mp3);
    let cover = jpeg(300, 10);
    let one = library.song("Discovery", "One More Time", Some(&cover), None, true);
    let two = library.song("Discovery", "Aerodynamic", Some(&cover), None, true);
    library.song("Homework", "Da Funk", Some(&cover), None, true);
    Mock::given(method("GET"))
        .and(path("/api/song"))
        .and(header("X-Nd-Authorization", "Bearer jwt-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"albumId":"al-disc","path":"One More Time.mp3"},{"albumId":"al-disc","path":"/Aerodynamic.mp3"},{"albumId":"","path":"Da Funk.mp3"}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/getCoverArt"))
        .and(query_param("id", "al-al-disc"))
        .and(query_param("u", "admin"))
        .and(query_param("size", "300"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "image/jpeg")
                .set_body_bytes(b"tile".to_vec()),
        )
        .mount(&server)
        .await;
    let (settings, identity) = navidrome(&server);
    let services = CoverUpgradeServices {
        library: None,
        identity: Some(identity),
        http: Some(reqwest::Client::new()),
    };
    let (worker, store) = library.worker_with(FixedFinder::new(None), false, services, settings);

    run(&worker, &store, whole(CoverUpgradeMode::Scan)).await;

    let current = store.current();
    let items: Vec<(Option<&str>, usize)> = current
        .queue
        .iter()
        .map(|item| {
            (
                item.navidrome_album_id.as_deref(),
                item.files.as_ref().map_or(0, Vec::len),
            )
        })
        .collect();
    assert_eq!(items, [(Some("al-disc"), 2), (None, 1)]);
    // Every album counts as done once some are known, the folder's too (as the C# counted).
    assert_eq!((current.albums_total, current.albums_done), (1, 2));
    assert_eq!((current.songs_total, current.songs_read), (3, 3));
    let discovery = current
        .preview
        .iter()
        .find(|row| row.navidrome_album_id.is_some())
        .expect("listed");
    let mut paths = discovery.paths.clone().unwrap_or_default();
    paths.sort();
    let mut expected = vec![one, two];
    expected.sort();
    assert_eq!(paths, expected);

    assert_eq!(
        worker.navidrome_thumbnail(&discovery.id).await,
        Some((b"tile".to_vec(), "image/jpeg".to_string()))
    );
    let other = current
        .preview
        .iter()
        .find(|row| row.navidrome_album_id.is_none())
        .expect("listed");
    assert_eq!(worker.navidrome_thumbnail(&other.id).await, None);
}
