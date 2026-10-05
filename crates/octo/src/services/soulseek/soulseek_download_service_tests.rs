//! The SoulseekDownloadService tests: SoulseekDenyListTests, SoulseekResolveRetryTests,
//! SoulseekIncompleteFolderTests, the ResolveInJob and job folder tests of ParallelDownloadTests,
//! TranscodeDecisionTests' WeighTranscode, and AlbumFolderTests' album walk through the real
//! service against a fake slskd (a wiremock responder with the C# fake's state).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{AppSettings, DownloadSource, SettingsStore, SoulseekSettings, SubsonicSettings};
use octo_core::soulseek::{RoutingKind, SoulseekFileHit, SoulseekRouting};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::common::{DownloadConcurrency, DownloadCore, DownloadServices};
use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::i_download_service::IDownloadService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::soulseek_client::Timings;
use crate::services::subsonic::NavidromeIdentityService;
use octo_media::audio::AudioFingerprinter;

/// A folder under the temp directory, removed when the test ends.
struct Root(tempfile::TempDir);

impl Root {
    fn new(prefix: &str) -> Self {
        Root(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("a temp folder"),
        )
    }

    fn path(&self) -> String {
        self.0.path().to_string_lossy().into_owned()
    }

    fn write(&self, relative: &str, size: usize) -> String {
        let path = self.0.path().join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the folder is made");
        std::fs::write(&path, vec![0u8; size]).expect("the file is written");
        path.to_string_lossy().into_owned()
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// ---- SoulseekDenyListTests -----------------------------------------------------------------
//
// CandidateAllowed is the seam between the deny-list and the ranking. The failure it guards
// against is invisible from outside: a filter that denies everything leaves every track
// unfetchable and looks exactly like Soulseek having no copies.

fn hit(user: &str, file: &str) -> SoulseekFileHit {
    SoulseekFileHit {
        username: user.into(),
        filename: file.into(),
        extension: "flac".into(),
        size: 40_000_000,
        ..Default::default()
    }
}

#[test]
fn candidate_allowed_rejected_candidate_is_never_offered_again() {
    let registry = RejectedPeerRegistry::in_memory();
    registry.deny(Some("peer1"), Some("a.flac"), "wrong recording", "A - B");

    assert!(!candidate_allowed(&hit("peer1", "a.flac"), Some(&registry), true));
    assert!(candidate_allowed(&hit("peer1", "b.flac"), Some(&registry), true));
}

/// Turning the setting off is the fastest recovery from a wrong denial, so it has to work
/// without touching the file the denials live in.
#[test]
fn candidate_allowed_verification_off_the_list_is_inert() {
    let registry = RejectedPeerRegistry::in_memory();
    registry.deny(Some("peer1"), Some("a.flac"), "wrong recording", "A - B");

    assert!(candidate_allowed(&hit("peer1", "a.flac"), Some(&registry), false));
}

#[test]
fn candidate_allowed_no_registry_filters_nothing() {
    assert!(candidate_allowed(&hit("peer1", "a.flac"), None, true));
}

// ---- SoulseekResolveRetryTests -------------------------------------------------------------
//
// slskd marks a transfer Succeeded before moving the file out of its incomplete
// directory, and on bind mounts that move is a copy that can take seconds. The
// one-shot disk check used to miss the mid-move file, fail the attempt, and
// re-download the same track from the next peer. These tests pin the bounded
// re-poll that closes that window.

#[tokio::test]
async fn resolves_immediately_without_waiting() {
    let mut calls = 0;
    let result = retry_resolve(
        || {
            calls += 1;
            Some("/music/song.flac".to_string())
        },
        Duration::from_secs(30),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result.as_deref(), Some("/music/song.flac"));
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn resolves_when_file_appears_mid_window() {
    let mut calls = 0;
    let result = retry_resolve(
        || {
            calls += 1;
            (calls >= 3).then(|| "/music/song.flac".to_string())
        },
        Duration::from_secs(30),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result.as_deref(), Some("/music/song.flac"));
    assert_eq!(calls, 3);
}

#[tokio::test]
async fn gives_up_after_max_wait() {
    let result = retry_resolve(
        || None,
        Duration::from_millis(100),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result, None);
}

#[tokio::test]
async fn cancelled_caller_gets_one_final_check_instead_of_the_window() {
    let cts = CancellationToken::new();
    cts.cancel();

    let mut calls = 0;
    let result = retry_resolve(
        || {
            calls += 1;
            (calls >= 2).then(|| "/music/song.flac".to_string())
        },
        Duration::from_secs(30),
        Duration::from_secs(30),
        &cts,
    )
    .await;

    // First check misses, the delay is cancelled, the final check lands.
    assert_eq!(result.as_deref(), Some("/music/song.flac"));
    assert_eq!(calls, 2);
}

// ---- SoulseekIncompleteFolderTests ---------------------------------------------------------
//
// slskd marks a transfer Succeeded before moving it out of its incomplete folder, and a
// full-size copy there used to be taken as the download, then deleted under Octo (#69).

const INCOMPLETE_REMOTE: &str = r"Music\Artist\Album\13 - Song.flac";
const INCOMPLETE_LEAF: &str = "13 - Song.flac";
const INCOMPLETE_SIZE: usize = 200_000;

fn resolve_incomplete(root: &Root, remote: &str, excluded: Option<Vec<String>>) -> Option<String> {
    resolve_local_path(
        remote,
        INCOMPLETE_SIZE as i64,
        false,
        &[root.path()],
        &excluded.unwrap_or_else(|| names(&[DEFAULT_INCOMPLETE_FOLDER_NAME])),
        None,
    )
}

#[test]
fn a_full_size_copy_in_the_incomplete_folder_is_never_the_answer() {
    let root = Root::new("octo-incomplete-");
    let partial = root.write(
        &format!("slskd/incomplete/peer/Music/Artist/Album/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    assert_eq!(resolve_incomplete(&root, INCOMPLETE_REMOTE, None), None);
    // The same file with nothing excluded is found, so the None above is the exclusion at work.
    assert_eq!(
        resolve_incomplete(&root, INCOMPLETE_REMOTE, Some(Vec::new())),
        Some(partial)
    );
}

#[test]
fn an_incomplete_folder_named_by_slskd_is_excluded_too() {
    let root = Root::new("octo-incomplete-");
    root.write(
        &format!("slskd/partial/peer/Music/Artist/Album/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    assert_eq!(
        resolve_incomplete(
            &root,
            INCOMPLETE_REMOTE,
            Some(excluded_folder_names(Some(r"D:\slskd\partial\")))
        ),
        None
    );
}

#[test]
fn a_peers_own_folder_called_incomplete_is_still_found() {
    let root = Root::new("octo-incomplete-");
    let final_path = root.write(&format!("slskd/incomplete/{INCOMPLETE_LEAF}"), INCOMPLETE_SIZE);
    assert_eq!(
        resolve_incomplete(&root, r"Music\Artist\incomplete\13 - Song.flac", None),
        Some(final_path)
    );
}

#[test]
fn excluded_names() {
    let cases: [(Option<&str>, &[&str]); 3] = [
        (None, &["incomplete", ".octo-incoming"]),
        (Some("/app/incomplete"), &["incomplete", ".octo-incoming"]),
        (
            Some(r"D:\slskd\Partial\"),
            &["incomplete", "Partial", ".octo-incoming"],
        ),
    ];
    for (configured, expected) in cases {
        assert_eq!(
            excluded_folder_names(configured),
            names(expected),
            "configured {configured:?}"
        );
    }
}

#[test]
fn a_file_in_another_downloads_job_folder_is_never_found_by_name() {
    let root = Root::new("octo-incomplete-");
    // Another download's job folder holds a file with the very same name and size.
    root.write(
        &format!(".octo-incoming/slskd/0123456789abcdef/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    assert_eq!(
        resolve_incomplete(&root, INCOMPLETE_REMOTE, Some(excluded_folder_names(None))),
        None
    );
}

#[tokio::test]
async fn the_wait_outlasts_slskds_move_and_returns_the_final_path() {
    let root = Root::new("octo-incomplete-");
    let partial = root.write(
        &format!("slskd/incomplete/peer/Music/Artist/Album/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    let final_path = format!("{}/slskd/Album/{INCOMPLETE_LEAF}", root.path());
    let (from, to) = (partial.clone(), final_path.clone());
    let mover = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let to_dir = std::path::Path::new(&to)
            .parent()
            .expect("a parent")
            .to_path_buf();
        std::fs::create_dir_all(&to_dir).expect("made");
        std::fs::rename(&from, &to).expect("moved");
        // slskd removes the emptied folder
        std::fs::remove_dir(std::path::Path::new(&from).parent().expect("a parent")).expect("removed");
    });

    let roots = [root.path()];
    let excluded = names(&["incomplete"]);
    let result = retry_resolve(
        || {
            resolve_local_path(
                INCOMPLETE_REMOTE,
                INCOMPLETE_SIZE as i64,
                false,
                &roots,
                &excluded,
                None,
            )
        },
        Duration::from_secs(5),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;
    mover.await.expect("the mover finishes");

    assert_eq!(result.as_deref(), Some(final_path.as_str()));
    assert!(std::path::Path::new(&final_path).is_file());
}

#[test]
fn a_copy_slskd_renamed_on_a_clash_is_found() {
    let root = Root::new("octo-incomplete-");
    // slskd names the newcomer <name>_<DateTime.UtcNow.Ticks><ext> when the name is taken.
    root.write(&format!("slskd/Album/{INCOMPLETE_LEAF}"), 1_000); // the older file that took the name
    let renamed = root.write("slskd/Album/13 - Song_638950000000000000.flac", INCOMPLETE_SIZE);
    assert_eq!(resolve_incomplete(&root, INCOMPLETE_REMOTE, None), Some(renamed));
}

#[test]
fn a_name_that_only_looks_renamed_is_not() {
    let root = Root::new("octo-incomplete-");
    root.write("slskd/Album/13 - Song_2.flac", INCOMPLETE_SIZE);
    root.write("slskd/Other/13 - Song_638950000000000000.flac", INCOMPLETE_SIZE);
    assert_eq!(resolve_incomplete(&root, INCOMPLETE_REMOTE, None), None);
}

// ---- ParallelDownloadTests: ResolveInJob and the job folders -------------------------------
//
// Downloads side by side were unsafe while a finished file was found by its name anywhere under
// the music folder. Each Soulseek download now lands in a job folder of its own; these pin down
// finding it there and nowhere else.

const JOB_REMOTE: &str = r"Music\Artist\Album\03 - Song.flac";
const JOB: &str = ".octo-incoming/slskd/aaaa";

#[test]
fn the_file_in_its_own_job_folder_is_found() {
    let root = Root::new("octo-parallel-");
    let mine = root.write(&format!("{JOB}/03 - Song.flac"), 1000);
    assert_eq!(
        resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false),
        Some(mine)
    );
}

#[test]
fn slskds_renamed_copy_inside_the_job_folder_is_found() {
    let root = Root::new("octo-parallel-");
    let renamed = root.write(&format!("{JOB}/03 - Song_638631234567890123.flac"), 1000);
    assert_eq!(
        resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false),
        Some(renamed)
    );
}

#[test]
fn a_cleaned_up_name_is_found_by_its_exact_size() {
    let root = Root::new("octo-parallel-");
    let cleaned = root.write(&format!("{JOB}/03 _ Song.flac"), 1000);
    assert_eq!(
        resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false),
        Some(cleaned)
    );
}

#[test]
fn the_same_file_in_another_job_or_the_music_root_is_never_taken() {
    let root = Root::new("octo-parallel-");
    root.write(".octo-incoming/slskd/bbbb/03 - Song.flac", 1000);
    root.write("Music/Artist/Album/03 - Song.flac", 1000);
    root.write("03 - Song.flac", 1000);
    assert_eq!(resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false), None);
}

#[test]
fn an_interrupted_transfer_needs_the_exact_size() {
    let root = Root::new("octo-parallel-");
    root.write(&format!("{JOB}/03 - Song.flac"), 1000 - 10);
    assert!(resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false).is_some());
    assert_eq!(resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, true), None);
}

#[test]
fn job_folders_are_dot_folders_and_unique() {
    let a = new_job_dir();
    let b = new_job_dir();
    assert!(a.starts_with(".octo-incoming/slskd/"), "{a}");
    assert_ne!(a, b);
    assert!(!a.contains(".."), "{a}");
}

// ---- TranscodeDecisionTests: WeighTranscode ------------------------------------------------

fn held() -> String {
    format!("{}/music/Artist/Song.flac", std::env::temp_dir().display())
}

fn other() -> String {
    format!("{}/music/Peer/Song.flac", std::env::temp_dir().display())
}

#[test]
fn the_first_transcode_is_held_back() {
    assert_eq!(
        weigh_transcode(None, None, &other(), Some(16900.0)),
        ReserveChoice::Hold
    );
}

/// The higher cutoff was made from the higher bitrate, so it replaces the one held.
#[test]
fn a_higher_cutoff_replaces_the_one_held() {
    assert_eq!(
        weigh_transcode(Some(&held()), Some(16900.0), &other(), Some(20350.0)),
        ReserveChoice::Hold
    );
}

#[test]
fn a_no_better_transcode_is_discarded() {
    for cutoff in [16900.0, 15000.0] {
        assert_eq!(
            weigh_transcode(Some(&held()), Some(16900.0), &other(), Some(cutoff)),
            ReserveChoice::DiscardNew,
            "cutoff {cutoff}"
        );
    }
}

/// The resolver can find the file already held back for a later peer offering the
/// same rip. Deleting it as a new, worse copy would lose the only copy there is.
#[test]
fn the_copy_already_held_is_never_discarded_as_a_new_one() {
    assert_eq!(
        weigh_transcode(Some(&held()), Some(16900.0), &held(), Some(16900.0)),
        ReserveChoice::AlreadyHeld
    );
}

// ---- AlbumFolderTests: the walk, through the real download service against a fake slskd ----

/// A FLAC whose STREAMINFO says it lasts this long, so the length check passes.
fn flac(seconds: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let (sample_rate, channels_minus_one, bits_minus_one): (u64, u64, u64) = (44100, 1, 15);
    let total_samples = 44100u64 * seconds;
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

type Answers = Arc<dyn Fn(&str) -> Vec<Value> + Send + Sync>;
type Folders = Arc<dyn Fn(&str, &str) -> Option<Value> + Send + Sync>;

#[derive(Default)]
struct SlskdState {
    searches: Vec<String>,
    batches: Vec<(String, Vec<String>, String)>,
    browses: Vec<(String, String)>,
    search_text: HashMap<String, String>,
    transfers: HashMap<String, Vec<Value>>,
    lengths: HashMap<String, u64>,
    ids: i32,
    /// An slskd without batch downloads: the old one-file enqueue, filed by the peer's folder.
    no_batches: bool,
    /// The files the old enqueue was asked for.
    enqueued: Vec<String>,
    /// What the yt-dlp shim was asked to fetch.
    you_tube_downloads: Vec<String>,
}

/// slskd as far as one walk needs: logged in, searches answered from a script, and every batch
/// file written straight into its destination folder and reported finished.
#[derive(Clone)]
struct FakeSlskd {
    root: String,
    responses_for: Answers,
    folders: Arc<Mutex<Option<Folders>>>,
    state: Arc<Mutex<SlskdState>>,
}

impl FakeSlskd {
    fn length(&self, remote: &str, seconds: i32) {
        self.state
            .lock()
            .lengths
            .insert(remote.to_string(), seconds as u64);
    }

    /// What a peer lists for one folder of its share, by user and folder; None lists nothing.
    fn set_folders(&self, folders: impl Fn(&str, &str) -> Option<Value> + Send + Sync + 'static) {
        *self.folders.lock() = Some(Arc::new(folders));
    }

    fn searches(&self) -> Vec<String> {
        self.state.lock().searches.clone()
    }

    fn batches(&self) -> Vec<(String, Vec<String>, String)> {
        self.state.lock().batches.clone()
    }

    fn browses(&self) -> Vec<(String, String)> {
        self.state.lock().browses.clone()
    }
}

fn json_answer(body: &Value, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string().into_bytes(), "application/json")
}

impl Respond for FakeSlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let segments: Vec<&str> = path.split('/').collect();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = request.method.as_str();
        if path == "/api/v0/session" {
            return json_answer(&json!({"token": "jwt", "expires": 4102444800i64}), 200);
        }
        if path == "/api/v0/options" {
            return json_answer(
                &json!({"directories": {"downloads": self.root, "incomplete": "/app/incomplete"}}),
                200,
            );
        }
        if path == "/api/v0/searches" && method == "POST" {
            let text = body["searchText"].as_str().unwrap_or_default().to_string();
            let id = body["id"].as_str().unwrap_or_default().to_string();
            let mut state = self.state.lock();
            state.search_text.insert(id, text.clone());
            state.searches.push(text);
            return json_answer(&json!({}), 200);
        }
        if path.starts_with("/api/v0/searches/") && path.ends_with("/responses") {
            let text = self
                .state
                .lock()
                .search_text
                .get(segments[4])
                .cloned()
                .unwrap_or_default();
            return json_answer(&Value::Array((self.responses_for)(&text)), 200);
        }
        if path.starts_with("/api/v0/searches/") {
            return json_answer(
                &json!({"state": "Completed, ResponseLimitReached", "endedAt": "2026-10-03T12:00:00Z", "responseCount": 1}),
                200,
            );
        }
        if path.starts_with("/api/v0/users/") && path.ends_with("/directory") && method == "POST" {
            let user = segments[4].to_string();
            let folder = body["directory"].as_str().unwrap_or_default().to_string();
            self.state.lock().browses.push((user.clone(), folder.clone()));
            let folders = self.folders.lock().clone();
            return match folders.and_then(|f| f(&user, &folder)) {
                Some(listing) => json_answer(&listing, 200),
                None => ResponseTemplate::new(404),
            };
        }
        if path == "/api/v0/transfers/downloads/batches" && self.state.lock().no_batches {
            return ResponseTemplate::new(404);
        }
        if path.starts_with("/api/v0/transfers/downloads/")
            && path != "/api/v0/transfers/downloads/batches"
            && method == "POST"
        {
            // The old way: slskd files the download under the peer's own folder name.
            let user = segments[5].to_string();
            let mut state = self.state.lock();
            for request in body.as_array().cloned().unwrap_or_default() {
                let file = request["filename"].as_str().unwrap_or_default().to_string();
                let normalized = file.replace('\\', "/");
                let parts: Vec<&str> = normalized.split('/').collect();
                let leaf = parts[parts.len() - 1];
                let parent = parts[parts.len() - 2];
                let local = std::path::Path::new(&self.root).join(parent).join(leaf);
                std::fs::create_dir_all(local.parent().expect("a parent")).expect("made");
                let bytes = flac(state.lengths.get(&file).copied().unwrap_or(200));
                std::fs::write(&local, &bytes).expect("written");
                state.enqueued.push(file.clone());
                let transfer = json!({
                    "filename": file, "state": "Completed, Succeeded",
                    "size": bytes.len() as i64, "bytesTransferred": bytes.len() as i64,
                });
                state.transfers.entry(user.clone()).or_default().push(transfer);
            }
            return json_answer(&json!({}), 201);
        }
        if path == "/search" {
            return json_answer(
                &json!({"video_id": "vid123", "title": "Started", "duration": 180}),
                200,
            );
        }
        if path == "/download" {
            let dest = request
                .url
                .query_pairs()
                .find(|(k, _)| k == "dest")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let file = format!("{dest}.mp3");
            std::fs::create_dir_all(std::path::Path::new(&file).parent().expect("a parent")).expect("made");
            std::fs::write(&file, crate::services::test_support::mp3()).expect("written");
            self.state.lock().you_tube_downloads.push(file.clone());
            return json_answer(&json!({"path": file}), 200);
        }
        if path == "/api/v0/transfers/downloads/batches" {
            let user = body["username"].as_str().unwrap_or_default().to_string();
            let destination = body["options"]["destination"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let files: Vec<String> = body["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .map(|f| f["filename"].as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let mut state = self.state.lock();
            state
                .batches
                .push((user.clone(), files.clone(), destination.clone()));
            let mut queued = Vec::new();
            for file in &files {
                let leaf = file
                    .replace('\\', "/")
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let local = std::path::Path::new(&self.root).join(&destination).join(&leaf);
                std::fs::create_dir_all(local.parent().expect("a parent")).expect("made");
                let bytes = flac(state.lengths.get(file).copied().unwrap_or(200));
                std::fs::write(&local, &bytes).expect("written");
                state.ids += 1;
                let transfer = json!({
                    "id": format!("t-{}", state.ids), "filename": file, "state": "Completed, Succeeded",
                    "size": bytes.len() as i64, "bytesTransferred": bytes.len() as i64, "percentComplete": 100.0,
                });
                state
                    .transfers
                    .entry(user.clone())
                    .or_default()
                    .push(transfer.clone());
                queued.push(transfer);
            }
            return json_answer(
                &json!({"batch": {"username": user, "transfers": queued}, "failures": []}),
                201,
            );
        }
        if path.starts_with("/api/v0/transfers/downloads/") && method == "GET" {
            let user = segments[5].to_string();
            let files = self
                .state
                .lock()
                .transfers
                .get(&user)
                .cloned()
                .unwrap_or_default();
            return json_answer(
                &json!({"username": user, "directories": [{"directory": "x", "files": files}]}),
                200,
            );
        }
        json_answer(&json!({}), 200)
    }
}

struct Walk {
    service: Arc<BaseDownloadService>,
    slskd: FakeSlskd,
    songs: Vec<Song>,
    album_id: String,
    _server: MockServer,
}

const TITLES: [(&str, i32); 4] = [
    ("Intro", 90),
    ("Hold On", 200),
    ("Hold On, We're Going Home", 228),
    ("Started", 180),
];

async fn build(root: &Root, album_folders: bool, answers: impl FnOnce(Vec<Song>) -> Answers) -> Walk {
    build_with(root, album_folders, DownloadSource::Soulseek, answers).await
}

async fn build_with(
    root: &Root,
    album_folders: bool,
    source: DownloadSource,
    answers: impl FnOnce(Vec<Song>) -> Answers,
) -> Walk {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let songs: Vec<Song> = TITLES
        .iter()
        .enumerate()
        .map(|(i, (title, seconds))| {
            let id = registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Drake".into()),
                title: Some(title.to_string()),
                album: Some("Nothing Was the Same".into()),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            });
            Song {
                id: format!("ext-soulseek-{id}"),
                external_provider: Some("soulseek".into()),
                external_id: Some(id),
                title: title.to_string(),
                artist: "Drake".into(),
                album: "Nothing Was the Same".into(),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            }
        })
        .collect();

    let slskd = FakeSlskd {
        root: root.path(),
        responses_for: answers(songs.clone()),
        folders: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(SlskdState::default())),
    };
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(slskd.clone())
        .mount(&server)
        .await;

    let soulseek = SoulseekSettings {
        base_url: Some(server.uri()),
        username: Some("u".into()),
        password: Some("p".into()),
        min_file_size_bytes: 0,
        album_folders,
        parallel_downloads: 3,
        detect_transcodes: false,
        verify_downloads: false,
        download_timeout_seconds: 30,
        ..Default::default()
    };
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: soulseek.clone(),
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            download_source: source,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root.path()));
    let client = SoulseekClient::with_timings(
        &soulseek,
        Timings {
            search_poll_interval: Duration::from_millis(5),
            poll_interval: Duration::from_millis(5),
            min_search_spacing: Duration::ZERO,
            ..Default::default()
        },
    );

    let metadata = FakeMetadata::default();
    let album = Album {
        id: "album-1".into(),
        title: "Nothing Was the Same".into(),
        artist: "Drake".into(),
        songs: songs.clone(),
        ..Default::default()
    };
    metadata
        .albums
        .lock()
        .insert(("soulseek".into(), "album-1".into()), album);
    // A song whose id the registry never minted: nothing says what to search for.
    metadata.songs.lock().insert(
        ("soulseek".into(), "unknown-id".into()),
        Song {
            title: "Mystery".into(),
            artist: "Nobody".into(),
            external_provider: Some("soulseek".into()),
            external_id: Some("unknown-id".into()),
            ..Default::default()
        },
    );
    for song in &songs {
        metadata.songs.lock().insert(
            ("soulseek".into(), song.external_id.clone().expect("an id")),
            song.clone(),
        );
    }

    let core = DownloadCore {
        settings: Arc::clone(&settings),
        local_library: Arc::new(FakeLocalLibrary::default()),
        metadata: Arc::new(metadata),
        navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
        history: Arc::new(DownloadHistoryService::new(root.0.path().join("history.json"))),
        notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
    };
    let services = DownloadServices {
        concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
        ..Default::default()
    };
    let verification = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        Arc::new(AcoustIdClient::new(Arc::new(AcoustIdRateLimitHandler::new(
            Arc::new(AcoustIdRateLimiter::new()),
        )))),
        Arc::clone(&settings),
        None,
        None,
    ));
    let parts = SoulseekDownloadParts {
        slskd: client,
        rejected_peers: Arc::new(RejectedPeerRegistry::in_memory()),
        verification,
        youtube: Arc::new(YouTubeResolver::with_base_url(Some(&server.uri()))),
        id_registry: registry,
        soulseek_link: None,
        lidarr_imports: None,
        lidarr_fetcher: None,
    };
    let (service, _) = SoulseekDownloadService::build(core, services, parts);
    Walk {
        service,
        slskd,
        songs,
        album_id: "album-1".into(),
        _server: server,
    }
}

fn response(user: &str, files: &[(String, i32)]) -> Value {
    json!({
        "username": user, "uploadSpeed": 1_000_000, "queueLength": 0, "hasFreeUploadSlot": true,
        "files": files.iter().map(|(file, seconds)| json!({
            "filename": file, "size": flac(*seconds as u64).len() as i64, "length": seconds, "extension": "flac",
        })).collect::<Vec<Value>>(),
    })
}

fn remote(folder: &str, song: &Song) -> String {
    format!(
        "{folder}\\{:02} - {}.flac",
        song.track.expect("a number"),
        song.title
    )
}

fn duration_of(song: &Song) -> i32 {
    song.duration.expect("a length")
}

/// Every FLAC under the root that is not in the staging folder.
fn placed_flacs(root: &Root) -> usize {
    files_under(&root.path())
        .expect("the root lists")
        .iter()
        .filter(|f| f.ends_with(".flac") && !f.contains(".octo-incoming"))
        .count()
}

async fn walk_album(walk: &Walk) -> bool {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.download_album_with_source(
            "soulseek",
            &walk.album_id,
            DownloadSource::Soulseek,
            false,
            &CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("the walk ends within a minute")
    .expect("the walk runs")
}

#[tokio::test]
async fn a_whole_folder_is_one_search_and_one_batch_and_every_song_lands() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |songs| {
        Arc::new(move |text: &str| {
            if text == "Drake Nothing Was the Same" {
                let files: Vec<(String, i32)> = songs
                    .iter()
                    .map(|s| (remote(r"Music\Drake\NWTS", s), duration_of(s)))
                    .collect();
                vec![response("albumpeer", &files)]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }

    assert!(walk_album(&walk).await);

    assert_eq!(walk.slskd.searches(), names(&["Drake Nothing Was the Same"]));
    let batches = walk.slskd.batches();
    assert_eq!(batches.len(), 1);
    let (user, files, destination) = &batches[0];
    assert_eq!(user, "albumpeer");
    assert_eq!(files.len(), 4);
    assert!(destination.starts_with(".octo-incoming/slskd/"), "{destination}");
    assert_eq!(placed_flacs(&root), 4);
}

#[tokio::test]
async fn a_song_the_folder_lacks_is_searched_on_its_own() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |songs| {
        Arc::new(move |text: &str| {
            if text == "Drake Nothing Was the Same" {
                let files: Vec<(String, i32)> = songs
                    .iter()
                    .take(3)
                    .map(|s| (remote(r"Music\NWTS", s), duration_of(s)))
                    .collect();
                vec![response("albumpeer", &files)]
            } else if text.contains("Started") {
                vec![response(
                    "songpeer",
                    &[(remote("Singles", &songs[3]), duration_of(&songs[3]))],
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    for song in walk.songs.iter().take(3) {
        walk.slskd.length(&remote(r"Music\NWTS", song), duration_of(song));
    }
    walk.slskd
        .length(&remote("Singles", &walk.songs[3]), duration_of(&walk.songs[3]));

    assert!(walk_album(&walk).await);

    let searches = walk.slskd.searches();
    assert_eq!(searches.len(), 2, "{searches:?}");
    assert!(searches.iter().any(|s| s.contains("Started")), "{searches:?}");
    let mut users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    users.sort();
    assert_eq!(users, names(&["albumpeer", "songpeer"]));
}

#[tokio::test]
async fn with_album_folders_off_every_song_is_searched_on_its_own() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, false, |songs| {
        Arc::new(move |text: &str| {
            let found = songs.iter().find(|s| {
                let first = s.title.split(',').next().unwrap_or_default();
                text.contains(first) && !(s.title == "Hold On" && text.contains("Home"))
            });
            match found {
                Some(song) => vec![response("peer", &[(remote("x", song), duration_of(song))])],
                None => Vec::new(),
            }
        })
    })
    .await;
    for song in &walk.songs {
        walk.slskd.length(&remote("x", song), duration_of(song));
    }

    walk_album(&walk).await;

    let searches = walk.slskd.searches();
    assert!(
        !searches.iter().any(|s| s == "Drake Nothing Was the Same"),
        "{searches:?}"
    );
    assert!(searches.len() >= 4, "{searches:?}");
    assert!(walk.slskd.batches().iter().all(|(_, files, _)| files.len() == 1));
}

fn lossy_response(user: &str, file: &str, seconds: i32) -> Value {
    json!({
        "username": user, "uploadSpeed": 1_000_000, "queueLength": 0, "hasFreeUploadSlot": true,
        "files": [{"filename": file, "size": 8_000_000i64, "length": seconds, "extension": "mp3", "bitRate": 320}],
    })
}

const NWTS_FOLDER: &str = r"Music\Drake\Nothing Was the Same";

/// #70: the search finds the song only as an MP3, and the FLAC sits beside it in the same
/// folder, unanswered. Octo asks the peer for that folder and takes the FLAC from it.
#[tokio::test]
async fn a_flac_beside_the_only_mp3_the_search_found_is_taken() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, false, |_| {
        Arc::new(|text: &str| {
            if text.contains("Started") {
                vec![lossy_response(
                    "bothpeer",
                    &format!("{NWTS_FOLDER}\\04 - Started.mp3"),
                    180,
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    let started = walk.songs[3].clone();
    let flac_remote = format!("{NWTS_FOLDER}\\04 - Started.flac");
    walk.slskd.length(&flac_remote, 180);
    walk.slskd.set_folders(|user, dir| {
        (user == "bothpeer" && dir == NWTS_FOLDER).then(|| {
            json!({
                "name": NWTS_FOLDER,
                "files": [
                    {"filename": "04 - Started.mp3", "size": 8_000_000i64, "extension": "mp3", "length": 180},
                    {"filename": "04 - Started.flac", "size": flac(180).len() as i64, "extension": "flac", "length": 180},
                    {"filename": "03 - Hold On, We're Going Home.flac", "size": flac(228).len() as i64, "extension": "flac", "length": 228},
                ],
            })
        })
    });

    let path = tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition(
            "soulseek",
            started.external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute")
    .expect("the FLAC is fetched");

    assert_eq!(
        walk.slskd.browses(),
        vec![("bothpeer".to_string(), NWTS_FOLDER.to_string())]
    );
    let batches = walk.slskd.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].1, vec![flac_remote]);
    assert!(path.ends_with(".flac"), "{path}");
    assert!(std::path::Path::new(&path).is_file());
}

#[tokio::test]
async fn no_flac_beside_the_mp3_means_no_download() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, false, |_| {
        Arc::new(|text: &str| {
            if text.contains("Started") {
                vec![lossy_response(
                    "mp3peer",
                    &format!("{NWTS_FOLDER}\\04 - Started.mp3"),
                    180,
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    walk.slskd.set_folders(|_, _| {
        Some(json!({
            "name": NWTS_FOLDER,
            "files": [{"filename": "04 - Started.mp3", "size": 8_000_000i64, "extension": "mp3", "length": 180}],
        }))
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition(
            "soulseek",
            walk.songs[3].external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute");

    let error = outcome.expect_err("nothing to download");
    // "No copy here" is what lets a library action try its next source.
    assert!(
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "{error:#}"
    );
    assert_eq!(walk.slskd.browses().len(), 1);
    assert!(walk.slskd.batches().is_empty());
}

// ---- Rust-only ---------------------------------------------------------------------------

#[test]
fn a_rejected_file_is_deleted_only_when_this_attempt_made_it() {
    let root = Root::new("octo-discard-");
    let path = root.write("song.flac", 10);
    // Started long after the file was made: not ours.
    discard_rejected_download(&path, Utc::now() + chrono::TimeDelta::hours(1));
    assert!(std::path::Path::new(&path).is_file());
    discard_rejected_download(&path, Utc::now());
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn a_missing_artist_or_title_is_an_invalid_operation() {
    let error: anyhow::Error = InvalidOperationException("Cannot download".into()).into();
    assert!(error.downcast_ref::<InvalidOperationException>().is_some());
    assert_eq!(error.to_string(), "Cannot download");
}

async fn acquire(walk: &Walk, external_id: &str, source: Option<DownloadSource>) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition(
            "soulseek",
            external_id,
            false,
            true,
            source,
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute")
}

/// The search for "Started" answers two peers; the first one's file is the wrong length.
fn two_peers(text: &str) -> Vec<Value> {
    if text.contains("Started") {
        vec![
            response("wrongpeer", &[(r"A\Drake\04 - Started.flac".to_string(), 180)]),
            response("rightpeer", &[(r"B\Drake\04 - Started.flac".to_string(), 180)]),
        ]
    } else {
        Vec::new()
    }
}

#[tokio::test]
async fn a_peer_that_delivers_the_wrong_length_is_passed_over_for_the_next() {
    let root = Root::new("octo-attempts-");
    let walk = build(&root, false, |_| Arc::new(two_peers)).await;
    // Advertised as 180 s, and 300 s when it arrives.
    walk.slskd.length(r"A\Drake\04 - Started.flac", 300);
    walk.slskd.length(r"B\Drake\04 - Started.flac", 180);

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the second peer's file is kept");

    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer", "rightpeer"]));
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    // The wrong file was this attempt's own, so it is gone; only the kept one is placed.
    assert_eq!(placed_flacs(&root), 1);
    assert_eq!(
        files_under(&root.path())
            .expect("lists")
            .iter()
            .filter(|f| f.ends_with(".flac"))
            .count(),
        1
    );
}

#[tokio::test]
async fn without_batches_the_file_is_found_by_name_the_old_way() {
    let root = Root::new("octo-attempts-");
    let walk = build(&root, false, |songs| {
        Arc::new(move |text: &str| {
            if text.contains("Started") {
                vec![response(
                    "oldpeer",
                    &[(remote(r"Music\Drake\NWTS", &songs[3]), duration_of(&songs[3]))],
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    walk.slskd.state.lock().no_batches = true;
    walk.slskd.length(
        &remote(r"Music\Drake\NWTS", &walk.songs[3]),
        duration_of(&walk.songs[3]),
    );

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("found by its name");

    assert_eq!(
        walk.slskd.state.lock().enqueued,
        vec![remote(r"Music\Drake\NWTS", &walk.songs[3])]
    );
    assert!(walk.slskd.batches().is_empty());
    assert!(
        path.ends_with(".flac") && std::path::Path::new(&path).is_file(),
        "{path}"
    );
}

#[tokio::test]
async fn soulseek_then_you_tube_falls_back_to_the_mp3_when_soulseek_has_nothing() {
    let root = Root::new("octo-attempts-");
    let walk = build_with(&root, false, DownloadSource::SoulseekThenYouTube, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the YouTube copy lands");

    assert_eq!(walk.slskd.state.lock().you_tube_downloads.len(), 1);
    assert!(
        path.ends_with(".mp3") && std::path::Path::new(&path).is_file(),
        "{path}"
    );
}

#[tokio::test]
async fn a_song_without_a_routing_is_an_invalid_operation() {
    let root = Root::new("octo-attempts-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;

    let error = acquire(&walk, "unknown-id", Some(DownloadSource::Soulseek))
        .await
        .expect_err("nothing to search for");

    assert!(
        error.downcast_ref::<InvalidOperationException>().is_some(),
        "{error:#}"
    );
    assert_eq!(
        error.to_string(),
        "Cannot download 'Nobody - Mystery': missing artist/title in external id"
    );
}
