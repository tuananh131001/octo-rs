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

use super::lyrics_sidecar_writer::LyricsTagAccess;

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
}

impl FakeTags {
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
}
