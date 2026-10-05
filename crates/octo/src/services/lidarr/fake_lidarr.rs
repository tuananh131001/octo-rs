//! `LidarrTrackFetcherTests.FakeLidarr`: enough of Lidarr's v1 API for one album (lookup, add,
//! monitor, search, tracks and files), served by a mock server. A search imports the files
//! `on_search` names, the way Lidarr would, onto the disk Octo sees.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// `LidarrTrackFetcherTests.AlbumId`.
pub(crate) const ALBUM_ID: &str = "rg-mezzanine";

#[derive(Clone)]
pub(crate) struct FakeLidarr {
    octo_root: PathBuf,
    state: Arc<Mutex<State>>,
}

struct State {
    added: bool,
    monitored: bool,
    next_file: i32,
    /// File id → (track id, Lidarr's path, quality).
    files: BTreeMap<i32, (i32, String, String)>,
    /// Track id → (title, number).
    tracks: BTreeMap<i32, (String, i32)>,
    knows_album: bool,
    /// What a search imports: (track id, title, quality). Empty: nothing arrives.
    on_search: Vec<(i32, String, String)>,
    searches: usize,
    deleted: Vec<i32>,
    monitor_changes: Vec<bool>,
}

impl FakeLidarr {
    /// The fake, and the server answering for it.
    pub async fn start(octo_root: impl Into<PathBuf>) -> (FakeLidarr, MockServer) {
        let fake = FakeLidarr {
            octo_root: octo_root.into(),
            state: Arc::new(Mutex::new(State {
                added: false,
                monitored: false,
                next_file: 100,
                files: BTreeMap::new(),
                tracks: BTreeMap::from([(1, ("Angel".to_string(), 1)), (3, ("Teardrop".to_string(), 3))]),
                knows_album: true,
                on_search: Vec::new(),
                searches: 0,
                deleted: Vec::new(),
                monitor_changes: Vec::new(),
            })),
        };
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(fake.clone()).mount(&server).await;
        (fake, server)
    }

    pub fn set_knows_album(&self, knows: bool) {
        self.state.lock().knows_album = knows;
    }

    /// `OnSearch = () => { Import(..); Import(..); }`.
    pub fn on_search(&self, imports: &[(i32, &str, &str)]) {
        self.state.lock().on_search = imports
            .iter()
            .map(|(id, title, quality)| (*id, title.to_string(), quality.to_string()))
            .collect();
    }

    pub fn searches(&self) -> usize {
        self.state.lock().searches
    }

    pub fn deleted(&self) -> Vec<i32> {
        self.state.lock().deleted.clone()
    }

    pub fn monitor_changes(&self) -> Vec<bool> {
        self.state.lock().monitor_changes.clone()
    }

    pub fn add_album(&self, monitored: bool) {
        let mut state = self.state.lock();
        state.added = true;
        state.monitored = monitored;
    }

    /// A file for a track, at Lidarr's path, written where Octo sees it. Returns Octo's path.
    pub fn import(&self, track_id: i32, title: &str, quality: &str) -> String {
        Self::import_into(&self.octo_root, &mut self.state.lock(), track_id, title, quality)
    }

    fn import_into(
        octo_root: &std::path::Path,
        state: &mut State,
        track_id: i32,
        title: &str,
        quality: &str,
    ) -> String {
        let extension = if quality.starts_with("FLAC") {
            ".flac"
        } else {
            ".mp3"
        };
        let relative = format!("Massive Attack/Mezzanine/{track_id:02} - {title}{extension}");
        let local = octo_root.join(&relative);
        std::fs::create_dir_all(local.parent().expect("a parent")).expect("the album folder");
        std::fs::write(&local, title).expect("the file");
        let id = state.next_file;
        state.next_file += 1;
        state.files.insert(
            id,
            (track_id, format!("/data/music/{relative}"), quality.to_string()),
        );
        local.to_string_lossy().into_owned()
    }

    fn album(state: &State, id: i32) -> Value {
        json!({
            "id": id, "foreignAlbumId": ALBUM_ID, "title": "Mezzanine", "monitored": state.monitored,
            "releaseDate": "1998-04-20",
            "artist": { "id": 5, "artistName": "Massive Attack", "foreignArtistId": "ma", "monitored": true },
        })
    }

    fn statistics(state: &State) -> Value {
        let mut album = Self::album(state, 7);
        album["statistics"] =
            json!({ "trackCount": state.tracks.len(), "trackFileCount": state.files.len() });
        album
    }

    fn tracks(state: &State) -> Value {
        Value::Array(
            state
                .tracks
                .iter()
                .map(|(id, (title, number))| {
                    let file = state.files.iter().find(|(_, f)| f.0 == *id);
                    json!({
                        "id": id, "title": title, "trackNumber": number.to_string(),
                        "duration": if *id == 1 { 380000 } else { 330000 },
                        "hasFile": file.is_some(), "trackFileId": file.map_or(0, |f| *f.0),
                        "artist": { "artistName": "Massive Attack" },
                    })
                })
                .collect(),
        )
    }

    fn files(state: &State) -> Value {
        Value::Array(
            state
                .files
                .iter()
                .map(|(id, (_, path, quality))| {
                    json!({ "id": id, "path": path, "size": 1000, "quality": { "quality": { "name": quality } } })
                })
                .collect(),
        )
    }

    fn json(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
    }
}

impl Respond for FakeLidarr {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        let mut state = self.state.lock();
        let state = &mut *state;
        match (request.method.as_str(), path) {
            ("GET", "/api/v1/album/lookup") => Self::json(if state.knows_album {
                json!([Self::album(state, 0)])
            } else {
                json!([])
            }),
            ("GET", "/api/v1/album") => Self::json(if state.added {
                json!([Self::album(state, 7)])
            } else {
                json!([])
            }),
            ("GET", "/api/v1/album/7") => Self::json(Self::statistics(state)),
            ("GET", "/api/v1/artist") => Self::json(json!([])),
            ("POST", "/api/v1/album") => {
                state.added = true;
                state.monitored = true;
                Self::json(Self::album(state, 7))
            }
            ("PUT", "/api/v1/album/7") => {
                state.monitored = true;
                state.monitor_changes.push(true);
                Self::json(Self::album(state, 7))
            }
            ("PUT", "/api/v1/album/monitor") => {
                let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
                let monitored = body["monitored"].as_bool().expect("monitored");
                state.monitored = monitored;
                state.monitor_changes.push(monitored);
                Self::json(Self::album(state, 7))
            }
            ("POST", "/api/v1/command") => {
                state.searches += 1;
                // Before the answer, the way a quick Lidarr would have it, so no test races the clock.
                for (track_id, title, quality) in state.on_search.clone() {
                    Self::import_into(&self.octo_root, state, track_id, &title, &quality);
                }
                Self::json(json!({ "id": 1, "name": "AlbumSearch" }))
            }
            ("GET", "/api/v1/track") => Self::json(Self::tracks(state)),
            ("GET", "/api/v1/trackFile") => Self::json(Self::files(state)),
            ("DELETE", _) if path.starts_with("/api/v1/trackfile/") => {
                let id: i32 = path["/api/v1/trackfile/".len()..].parse().expect("a file id");
                if let Some((_, lidarr_path, _)) = state.files.remove(&id) {
                    state.deleted.push(id);
                    let local = self.octo_root.join(&lidarr_path["/data/music/".len()..]);
                    let _ = std::fs::remove_file(local);
                }
                ResponseTemplate::new(200)
            }
            (method, _) => ResponseTemplate::new(404).set_body_string(format!(
                "no {method} {path}{}",
                request.url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )),
        }
    }
}
