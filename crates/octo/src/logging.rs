//! Logging: the `tracing` subscriber that stands in for ASP.NET's console logger, its level
//! filter, and the credential redaction `Program.cs` wrapped around the whole logger factory
//! (`builder.Logging.AddCredentialRedaction()`).
//!
//! Levels come from `RUST_LOG` when it is set. Otherwise the .NET variables are honoured:
//! `Logging__LogLevel__Default` and `Logging__LogLevel__<Category>`, with the .NET level names
//! (Trace, Debug, Information, Warning, Error, Critical, None). With neither, everything logs
//! at Information, as the shipped `appsettings.json` (no `Logging` section) did.
//!
//! Redaction happens on the formatted line, so it covers the message, every field and every
//! span field, whatever level an operator turns them up to.

use std::io::{self, Write};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The tracing target of the request lines ASP.NET's hosting layer wrote ("Request starting
/// ...", "Request finished ..."), kept so `Logging__LogLevel__Microsoft.AspNetCore` still turns
/// them off.
pub const HOSTING_DIAGNOSTICS: &str = "Microsoft.AspNetCore.Hosting.Diagnostics";

/// The tracing target of the host's lifetime lines ("Now listening on", "Application started").
pub const HOSTING_LIFETIME: &str = "Microsoft.Hosting.Lifetime";

/// What replaces a secret value.
pub const MASK: &str = "***";

/// The text with every secret query parameter's value replaced by [`MASK`]; see
/// [`octo_core::common::log_redaction`].
pub use octo_core::common::log_redaction::redact;

/// A writer that redacts each formatted event before passing it on. The fmt layer formats a
/// whole event into one buffer and writes it in one call, so the pattern always sees a complete
/// line.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        RedactingMakeWriter { inner }
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer_for(meta),
        }
    }
}

pub struct RedactingWriter<W> {
    inner: W,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match std::str::from_utf8(buf) {
            Ok(text) => self.inner.write_all(redact(text).as_bytes())?,
            // The formatter only ever writes UTF-8; anything else passes through untouched.
            Err(_) => self.inner.write_all(buf)?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A .NET `LogLevel` name as a tracing level directive. `None` turns a category off;
/// `Critical` has no tracing equivalent and maps to `error`.
pub fn dotnet_level(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "trace" => Some("trace"),
        "debug" => Some("debug"),
        "information" => Some("info"),
        "warning" => Some("warn"),
        "error" | "critical" => Some("error"),
        "none" => Some("off"),
        _ => None,
    }
}

/// The tracing targets a .NET log category stands for.
///
/// - `Octo` and `Octo.Some.Namespace` become the Rust crates and modules
///   (`octo::some::namespace`, also under `octo_core`, `octo_media` and `octo_subsonic`).
/// - `Microsoft.AspNetCore` (or a prefix of it) keeps its own name, which the ported request
///   and lifetime lines log under, and adds the Rust HTTP stack (`axum`, `tower_http`, `hyper`).
/// - `System.Net.Http` (or a prefix) adds the HTTP client (`reqwest`, `hyper_util`).
/// - Anything else is used as a target as written.
pub fn category_targets(category: &str) -> Vec<String> {
    let lower = category.to_ascii_lowercase();
    let mut targets = Vec::new();
    if lower == "octo" || lower.starts_with("octo.") {
        let rest: Vec<String> = category.split('.').skip(1).map(snake_case).collect();
        for krate in ["octo", "octo_core", "octo_media", "octo_subsonic"] {
            let mut path = vec![krate.to_string()];
            path.extend(rest.iter().cloned());
            targets.push(path.join("::"));
        }
        return targets;
    }
    let is_prefix_of = |full: &str| full.to_ascii_lowercase().starts_with(&lower);
    let mut matched_known = false;
    for known in [HOSTING_DIAGNOSTICS, HOSTING_LIFETIME] {
        if is_prefix_of(known) {
            // The category as the ported code spells it, so the case-sensitive target match works.
            targets.push(known[..category.len()].to_string());
            matched_known = true;
        }
    }
    if is_prefix_of("Microsoft.AspNetCore") {
        targets.extend(["axum", "tower_http", "hyper"].map(String::from));
        matched_known = true;
    }
    if is_prefix_of("System.Net.Http") {
        targets.extend(["reqwest", "hyper_util"].map(String::from));
        matched_known = true;
    }
    if !matched_known {
        targets.push(category.to_string());
    }
    targets.dedup();
    targets
}

/// `LastFm` → `last_fm`, `SubsonicProxyService` → `subsonic_proxy_service`.
fn snake_case(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len() + 4);
    let chars: Vec<char> = segment.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower = i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if prev_lower || (prev_upper && next_lower) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

/// The filter directives for an environment: `RUST_LOG` verbatim when set and not blank,
/// else the `Logging__LogLevel__*` variables translated, else `info`.
pub fn filter_directives(env: &[(String, String)]) -> String {
    if let Some((_, v)) = env.iter().find(|(k, v)| k == "RUST_LOG" && !v.trim().is_empty()) {
        return v.clone();
    }
    const PREFIX: &str = "logging__loglevel__";
    let mut default = "info";
    let mut directives = Vec::new();
    for (key, value) in env {
        let lower = key.to_ascii_lowercase();
        let Some(category) = lower.strip_prefix(PREFIX).map(|_| &key[PREFIX.len()..]) else {
            continue;
        };
        let Some(level) = dotnet_level(value) else {
            eprintln!("Ignoring {key}={value}: not a .NET log level");
            continue;
        };
        if category.eq_ignore_ascii_case("Default") {
            default = level;
        } else {
            directives.extend(
                category_targets(category)
                    .into_iter()
                    .map(|t| format!("{t}={level}")),
            );
        }
    }
    let mut all = vec![default.to_string()];
    all.extend(directives);
    all.join(",")
}

/// Installs the global subscriber: redacted output on stdout, filtered as
/// [`filter_directives`] decides from the process environment. Calling it twice is harmless.
pub fn init() {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let filter = EnvFilter::builder().parse_lossy(filter_directives(&env));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let fmt = tracing_subscriber::fmt::layer()
        .with_ansi(ansi)
        .with_writer(RedactingMakeWriter::new(std::io::stdout));
    let _ = tracing_subscriber::registry().with(filter).with(fmt).try_init();
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod tests;
