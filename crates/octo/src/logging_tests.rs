//! Port of `LogRedactionTests`: Subsonic credentials never reach the log. The request-line
//! tests live with the pipeline (`http/pipeline_tests.rs`), where requests flow.

use super::*;
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

/// Every byte the subscriber wrote, for searching.
#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture lock")).into_owned()
    }
}

pub(crate) struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.clone())
    }
}

/// A subscriber as `init` builds it, writing to `capture`, everything enabled.
pub(crate) fn capturing_subscriber(capture: &Capture) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::registry().with(EnvFilter::new("trace")).with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(RedactingMakeWriter::new(capture.clone())),
    )
}

pub(crate) const SECRET_NAMES: [&str; 16] = [
    "t", "s", "p", "apiKey", "token", "api_key", "client", "sk", "api_sig", "T", "APIKEY", "Token",
    "API_KEY", "Client", "user", "User",
];

pub(crate) fn secret() -> String {
    format!("Secret{}", uuid::Uuid::new_v4().simple())
}

#[test]
fn octos_own_lines_mask_each_secret_parameter() {
    for name in SECRET_NAMES {
        let capture = Capture::default();
        let secret = secret();
        let url = format!("http://navidrome:4533/rest/getSong?u=winters&{name}={secret}&id=7");
        let parsed = url::Url::parse(&url).expect("valid url");
        tracing::subscriber::with_default(capturing_subscriber(&capture), || {
            // A URL as a structured value, as a Url, and pasted straight into the message.
            tracing::warn!(url = %url, "relay failed");
            tracing::warn!(uri = %parsed, "relay failed");
            tracing::warn!("relay to {url} failed");
            // The unmasked form of HttpClient's own line.
            tracing::info!(target: "System.Net.Http.HttpClient.Default.ClientHandler", "Sending HTTP request GET {url}");
        });
        let text = capture.text();
        let own: Vec<&str> = text.lines().filter(|l| l.contains("rest/getSong")).collect();
        assert_eq!(own.len(), 4, "{name}: {text}");
        for line in own {
            assert!(
                line.contains(&format!("u=winters&{name}=***&id=7")),
                "{name}: {line}"
            );
        }
        assert!(!text.contains(&secret), "{name}: {text}");
    }
}

#[test]
fn scopes_are_masked_too() {
    let capture = Capture::default();
    let secret = secret();
    tracing::subscriber::with_default(capturing_subscriber(&capture), || {
        let uri = format!("http://navidrome:4533/rest/ping?u=winters&t={secret}");
        let span = tracing::info_span!("HTTP GET", uri = %uri);
        let _g = span.enter();
        tracing::info!("inside");
    });
    let text = capture.text();
    assert!(text.contains("inside"), "{text}");
    assert!(text.contains("t=***"), "{text}");
    assert!(!text.contains(&secret), "{text}");
}

#[test]
fn redact_masks_only_secret_values() {
    let cases = [
        ("/rest/ping?u=a&t=abc&s=def", "/rest/ping?u=a&t=***&s=***"),
        ("?p=enc:6162&u=a", "?p=***&u=a"),
        ("?apikey=K1&Token=K2#top", "?apikey=***&Token=***#top"),
        (
            "/2.0/?method=track.search&api_key=K1&format=json",
            "/2.0/?method=track.search&api_key=***&format=json",
        ),
        (
            "v2/lookup?client=K1&meta=recordings",
            "v2/lookup?client=***&meta=recordings",
        ),
        ("GET /rest/x?t=abc - 200", "GET /rest/x?t=*** - 200"),
        (
            "<a href='/rest/x?u=a&amp;t=abc'>",
            "<a href='/rest/x?u=a&amp;t=***'>",
        ),
        // Names that only start or end like a secret, and empty values, are left alone.
        (
            "?ts=1&st=2&sort=3&apiKeyId=4&tokens=5&clientId=6&api_keys=7&c=Octo",
            "?ts=1&st=2&sort=3&apiKeyId=4&tokens=5&clientId=6&api_keys=7&c=Octo",
        ),
        ("?t=&s=", "?t=&s="),
        ("no query here, t=abc", "no query here, t=abc"),
    ];
    for (text, expected) in cases {
        assert_eq!(redact(text), expected, "case {text:?}");
    }
}

#[test]
fn redact_returns_the_same_string_when_nothing_is_masked() {
    let text = "/rest/ping?u=winters&v=1.16.1";
    assert!(matches!(redact(text), Cow::Borrowed(t) if std::ptr::eq(t, text)));
}

#[test]
fn dotnet_levels_map_to_tracing_levels() {
    let cases = [
        ("Trace", Some("trace")),
        ("Debug", Some("debug")),
        ("Information", Some("info")),
        ("warning", Some("warn")),
        ("Error", Some("error")),
        ("Critical", Some("error")),
        ("None", Some("off")),
        ("Verbose", None),
    ];
    for (name, expected) in cases {
        assert_eq!(dotnet_level(name), expected, "case {name}");
    }
}

#[test]
fn filter_directives_prefer_rust_log_then_dotnet_variables_then_information() {
    let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(filter_directives(&env(&[])), "info");
    assert_eq!(
        filter_directives(&env(&[
            ("RUST_LOG", "octo=debug"),
            ("Logging__LogLevel__Default", "Error")
        ])),
        "octo=debug"
    );
    assert_eq!(
        filter_directives(&env(&[("LOGGING__LOGLEVEL__DEFAULT", "Warning")])),
        "warn"
    );
    let d = filter_directives(&env(&[
        ("Logging__LogLevel__Default", "Debug"),
        ("Logging__LogLevel__Microsoft.AspNetCore", "Warning"),
        ("Logging__LogLevel__Octo.Services.LastFm", "Trace"),
    ]));
    let parts: Vec<&str> = d.split(',').collect();
    assert_eq!(parts[0], "debug");
    for expected in [
        "Microsoft.AspNetCore=warn",
        "axum=warn",
        "hyper=warn",
        "octo::services::last_fm=trace",
        "octo_core::services::last_fm=trace",
    ] {
        assert!(parts.contains(&expected), "{expected} missing from {d}");
    }
    // The directive string parses, and the ported request lines obey the AspNetCore level.
    let filter = EnvFilter::builder().parse(&d).expect("directives parse");
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(RedactingMakeWriter::new(capture.clone())),
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: HOSTING_DIAGNOSTICS, "Request starting");
        tracing::warn!(target: HOSTING_DIAGNOSTICS, "a warning");
        tracing::debug!("a debug line");
    });
    let text = capture.text();
    assert!(!text.contains("Request starting"), "{text}");
    assert!(text.contains("a warning"), "{text}");
    assert!(text.contains("a debug line"), "{text}");
}

#[test]
fn categories_name_rust_targets() {
    assert_eq!(category_targets("Octo")[0], "octo");
    assert_eq!(
        category_targets("Octo.Services.Subsonic.SubsonicProxyService")[0],
        "octo::services::subsonic::subsonic_proxy_service"
    );
    assert!(category_targets("microsoft.aspnetcore").contains(&"Microsoft.AspNetCore".to_string()));
    assert!(category_targets("Microsoft").contains(&"Microsoft.Hosting.Lifetime"[..9].to_string()));
    assert!(category_targets("System.Net.Http").contains(&"reqwest".to_string()));
    assert_eq!(category_targets("notify"), vec!["notify".to_string()]);
}
