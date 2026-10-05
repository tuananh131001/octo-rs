//! What the ported service tests share: a mock server that answers by URL substring (the
//! Moq'd `HttpMessageHandler`s of the C# tests), and a capture of the log lines (`Mock<ILogger>`
//! verifications).

use std::collections::HashMap;
use std::sync::Arc;

use octo_core::common::dotnet;
use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// How a route's needle is compared with the request URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matching {
    /// `url.Contains(needle, OrdinalIgnoreCase)` on the URL as sent.
    IgnoreCase,
    /// `Uri.UnescapeDataString(url).Contains(needle, Ordinal)`, or `EndsWith` for a needle
    /// ending in `$` (QueryVariantLookupTests' harness).
    Unescaped,
}

/// Answers the first route whose needle the URL contains with that route's next body (the
/// last one repeats once they run out); anything else is a 404. Counts the calls per needle.
#[derive(Clone)]
pub struct Routes {
    routes: Arc<Vec<(String, Vec<String>)>>,
    counts: Arc<Mutex<HashMap<String, usize>>>,
    matching: Matching,
    not_found_body: Option<String>,
}

impl Routes {
    pub fn new(routes: Vec<(&str, Vec<&str>)>, matching: Matching) -> Self {
        Self {
            routes: Arc::new(
                routes
                    .into_iter()
                    .map(|(needle, bodies)| {
                        (
                            needle.to_string(),
                            bodies.into_iter().map(str::to_string).collect(),
                        )
                    })
                    .collect(),
            ),
            counts: Arc::new(Mutex::new(HashMap::new())),
            matching,
            not_found_body: None,
        }
    }

    /// One body per route, as the C# `Dictionary<needle, body>` harness had.
    pub fn single(routes: Vec<(&str, &str)>) -> Self {
        Self::new(
            routes
                .into_iter()
                .map(|(needle, body)| (needle, vec![body]))
                .collect(),
            Matching::IgnoreCase,
        )
    }

    pub fn with_not_found_body(mut self, body: &str) -> Self {
        self.not_found_body = Some(body.to_string());
        self
    }

    /// How many requests the route with this needle answered.
    pub fn calls(&self, needle: &str) -> usize {
        self.counts.lock().get(needle).copied().unwrap_or(0)
    }

    /// A mock server answering with these routes.
    pub async fn serve(&self) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(self.clone()).mount(&server).await;
        server
    }

    fn matches(&self, url: &str, needle: &str) -> bool {
        match self.matching {
            Matching::IgnoreCase => url.to_lowercase().contains(&needle.to_lowercase()),
            Matching::Unescaped => {
                let url = dotnet::unescape_data_string(url);
                match needle.strip_suffix('$') {
                    Some(end) => url.ends_with(end),
                    None => url.contains(needle),
                }
            }
        }
    }
}

impl Respond for Routes {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let url = request.url.to_string();
        for (needle, bodies) in self.routes.iter() {
            if !self.matches(&url, needle) {
                continue;
            }
            let mut counts = self.counts.lock();
            let n = counts.entry(needle.clone()).or_insert(0);
            let body = bodies[(*n).min(bodies.len() - 1)].clone();
            *n += 1;
            return ResponseTemplate::new(200).set_body_string(body);
        }
        match &self.not_found_body {
            Some(body) => ResponseTemplate::new(404).set_body_string(body.clone()),
            None => ResponseTemplate::new(404),
        }
    }
}

/// Every request the server received, in order.
pub async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

/// The log lines written while the guard is held, on this thread.
#[derive(Clone, Default)]
pub struct LogCapture {
    lines: Arc<Mutex<Vec<(Level, String)>>>,
}

impl LogCapture {
    /// Starts capturing on the current thread (a `#[tokio::test]` runs everything there).
    pub fn start() -> (LogCapture, DefaultGuard) {
        let capture = LogCapture::default();
        let subscriber = Registry::default().with(CaptureLayer {
            lines: Arc::clone(&capture.lines),
        });
        (capture, tracing::subscriber::set_default(subscriber))
    }

    pub fn at(&self, level: Level) -> Vec<String> {
        self.lines
            .lock()
            .iter()
            .filter(|(l, _)| *l == level)
            .map(|(_, line)| line.clone())
            .collect()
    }
}

struct CaptureLayer {
    lines: Arc<Mutex<Vec<(Level, String)>>>,
}

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        self.lines.lock().push((*event.metadata().level(), visitor.0));
    }
}

/// Whether ffmpeg is on the PATH, asked once, as `FfmpegFactAttribute` did. A test that needs it
/// returns early with a printed reason when it is missing.
pub fn ffmpeg_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Runs `ffmpeg -y -nostdin -hide_banner -v error <arguments>` in `dir`, panicking with ffmpeg's
/// own error when it fails.
pub fn run_ffmpeg(dir: &std::path::Path, arguments: &str) {
    let output = std::process::Command::new("ffmpeg")
        .args(["-y", "-nostdin", "-hide_banner", "-v", "error"])
        .args(arguments.split_whitespace())
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("ffmpeg starts");
    assert!(
        output.status.success(),
        "ffmpeg {arguments}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `AudioFixtures.Mp3`: twenty silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz, the smallest
/// file the tag writer opens as an MP3.
pub fn mp3() -> Vec<u8> {
    const FRAME_LENGTH: usize = 417;
    let mut bytes = vec![0u8; FRAME_LENGTH * 20];
    for frame in 0..20 {
        let offset = frame * FRAME_LENGTH;
        bytes[offset..offset + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    bytes
}

/// `AudioFixtures.Flac`: a FLAC with a STREAMINFO block describing two seconds of 16-bit stereo
/// and no frames.
pub fn flac() -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let (sample_rate, channels_minus_one, bits_minus_one, total_samples) = (44100u64, 1u64, 15u64, 88200u64);
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}
