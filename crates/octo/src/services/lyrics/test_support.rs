//! Test helpers for the lyrics sources: a local HTTP server standing in for the services, as
//! the C# tests' mocked `HttpMessageHandler` did, and fakes for the tags and the sources.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use octo_core::lyrics::{ILyricsSource, LyricsLookup, LyricsQuery};
use parking_lot::Mutex;
use percent_encoding::percent_decode_str;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::lyrics_sidecar_writer::{LyricsSongTags, LyricsTagAccess};

/// A server that answers every request through `respond`, given the request's URL with its
/// escapes undone (`Uri.UnescapeDataString`, as the C# tests compared them).
pub(crate) struct FakeHttp {
    server: MockServer,
}

impl FakeHttp {
    pub(crate) async fn start<F>(respond: F) -> Self
    where
        F: Fn(&str) -> ResponseTemplate + Send + Sync + 'static,
    {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(move |request: &Request| respond(&unescaped(request)))
            .mount(&server)
            .await;
        Self { server }
    }

    pub(crate) fn uri(&self) -> String {
        self.server.uri()
    }

    /// Every URL asked for, unescaped, in order.
    pub(crate) async fn calls(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(unescaped)
            .collect()
    }
}

fn unescaped(request: &Request) -> String {
    percent_decode_str(request.url.as_str())
        .decode_utf8_lossy()
        .into_owned()
}

pub(crate) fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(body)
}

pub(crate) fn status(code: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_string(body)
}

/// A source that answers every lookup the same way and counts them.
pub(crate) struct FakeSource {
    key: String,
    answer: Box<dyn Fn() -> LyricsLookup + Send + Sync>,
    calls: AtomicUsize,
}

impl FakeSource {
    pub(crate) fn new(key: &str, answer: impl Fn() -> LyricsLookup + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            key: key.to_string(),
            answer: Box::new(answer),
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ILyricsSource for FakeSource {
    fn key(&self) -> &str {
        &self.key
    }

    async fn find(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsLookup {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.answer)()
    }
}

/// The songs' tags, held in memory: what TagLib read and wrote on the C# tests' MP3 fixture.
#[derive(Default)]
pub(crate) struct FakeTags {
    lyrics: Mutex<HashMap<PathBuf, String>>,
    songs: Mutex<HashMap<PathBuf, LyricsSongTags>>,
}

impl FakeTags {
    /// The song's performer and title (`file.Tag.Performers = [artist]; file.Tag.Title = title`).
    pub(crate) fn tag(&self, path: &Path, artist: Option<&str>, title: &str) {
        self.songs.lock().insert(
            path.to_path_buf(),
            LyricsSongTags {
                first_performer: artist.map(str::to_string),
                title: Some(title.to_string()),
                ..LyricsSongTags::default()
            },
        );
    }

    pub(crate) fn set(&self, path: &Path, lyrics: &str) {
        self.lyrics.lock().insert(path.to_path_buf(), lyrics.to_string());
    }

    pub(crate) fn get(&self, path: &Path) -> Option<String> {
        self.lyrics.lock().get(path).cloned()
    }
}

impl LyricsTagAccess for FakeTags {
    fn read_lyrics(&self, path: &Path) -> std::io::Result<Option<String>> {
        std::fs::metadata(path)?;
        Ok(self.get(path))
    }

    fn write_lyrics(&self, path: &Path, lyrics: Option<&str>) -> anyhow::Result<()> {
        std::fs::metadata(path)?;
        let mut all = self.lyrics.lock();
        match lyrics {
            Some(text) => all.insert(path.to_path_buf(), text.to_string()),
            None => all.remove(path),
        };
        Ok(())
    }

    fn duration_seconds(&self, _path: &Path) -> Option<i32> {
        None
    }

    /// A file with no tags set is read as audio without any; a missing file is an I/O error.
    fn read_song(&self, path: &Path) -> std::io::Result<Option<LyricsSongTags>> {
        std::fs::metadata(path)?;
        let mut song = self.songs.lock().get(path).cloned().unwrap_or_default();
        song.lyrics = self.get(path);
        Ok(Some(song))
    }
}

/// A source whose answer depends on the query, which keeps the titles it was asked for and can
/// run something after each lookup (`LyricsChoiceTests.CountingSource`,
/// `LyricsLibraryStepsTests.Source`).
pub(crate) struct AskingSource {
    key: String,
    answer: Box<dyn Fn(&LyricsQuery) -> LyricsLookup + Send + Sync>,
    asked: Mutex<Vec<String>>,
    after: Mutex<Option<Box<dyn Fn(usize) + Send + Sync>>>,
}

impl AskingSource {
    pub(crate) fn new(
        key: &str,
        answer: impl Fn(&LyricsQuery) -> LyricsLookup + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            key: key.to_string(),
            answer: Box::new(answer),
            asked: Mutex::new(Vec::new()),
            after: Mutex::new(None),
        })
    }

    /// The titles asked for, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }

    /// Run after each lookup, given how many there have been; None stops it.
    pub(crate) fn set_after(&self, after: Option<Box<dyn Fn(usize) + Send + Sync>>) {
        *self.after.lock() = after;
    }
}

#[async_trait]
impl ILyricsSource for AskingSource {
    fn key(&self) -> &str {
        &self.key
    }

    async fn find(&self, query: &LyricsQuery, _ct: &CancellationToken) -> LyricsLookup {
        let asked = {
            let mut asked = self.asked.lock();
            asked.push(query.title.clone());
            asked.len()
        };
        let lookup = (self.answer)(query);
        if let Some(after) = self.after.lock().as_ref() {
            after(asked);
        }
        lookup
    }
}
