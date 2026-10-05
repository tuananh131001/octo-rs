//! Port of `Services/Updates/ReleaseCheck.cs`, the GitHub release check behind
//! `update/release.json`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::Clock;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::json::datetime;
use octo_core::settings::{SettingsStore, UpdateSettings};
use octo_core::updates::ReleaseVersion;
use parking_lot::Mutex;
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::services::state_file;

/// One dated server release, with the notes a person reads before updating.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ReleaseNote {
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub tag: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub name: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub notes: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub url: String,
    #[serde(with = "datetime::utc_option")]
    pub published_utc: Option<DateTime<Utc>>,
}

/// What the last check found, kept on disk so a restart answers without the network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ReleaseCheckState {
    pub repo: Option<String>,
    #[serde(with = "datetime::utc_option")]
    pub checked_utc: Option<DateTime<Utc>>,
    #[serde(with = "datetime::utc_option")]
    pub attempted_utc: Option<DateTime<Utc>>,
    pub error: Option<String>,
    #[serde(rename = "ETag")]
    pub e_tag: Option<String>,

    /// The server's releases, newest first.
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub releases: Vec<ReleaseNote>,
}

/// What the dashboard shows about new releases. Standing is how the running build compares
/// with the newest release: "behind", "current", "ahead" (a build cut after it), or "unknown"
/// (a build that names no release, or no release known yet).
///
/// The controller (6-B) owns how this is answered over the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseCheckView {
    pub enabled: bool,
    pub repo: String,
    pub running: String,
    pub latest: Option<ReleaseNote>,
    pub newer: Vec<ReleaseNote>,
    pub update_available: bool,
    pub standing: String,
    pub checked_utc: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// Asks GitHub whether a newer Octo release is out: every 6 hours, and when someone presses
/// Check now. Reads the release list rather than "latest", because the repo also publishes the
/// apps' releases and "latest" is a flag a person sets by hand; only dated tags count. A failed
/// check keeps the last good answer and says why, and never throws.
pub struct ReleaseCheck {
    path: PathBuf,
    http: reqwest::Client,
    api_base: String,
    /// `IOptionsMonitor<UpdateSettings>`: read at every use.
    settings: Arc<SettingsStore>,
    clock: Clock,
    running: String,
    /// `SemaphoreSlim(1, 1)`: one check at a time.
    gate: tokio::sync::Mutex<()>,
    state: Mutex<ReleaseCheckState>,
}

static REPO_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$").expect("a valid pattern"));

static NOT_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9.\-]").expect("a valid pattern"));

impl ReleaseCheck {
    pub const CLIENT_NAME: &'static str = "github-releases";

    pub const GITHUB_API: &'static str = "https://api.github.com";

    /// The named client's timeout.
    pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

    pub const INTERVAL: TimeDelta = TimeDelta::hours(6);

    /// Check now is honoured at most this often, so a busy button cannot spend GitHub's limit.
    pub const MANUAL_SPACING: TimeDelta = TimeDelta::minutes(1);

    const KEPT_RELEASES: usize = 10;
    const SHOWN_NEWER: usize = 5;
    const MAX_NOTES_LENGTH: usize = 20_000;

    /// The production check: GitHub's API through a 15 s client, the system clock, and the
    /// release this build is. Loads `path` now.
    pub fn new(path: impl AsRef<Path>, settings: Arc<SettingsStore>) -> Self {
        // IHttpClientFactory's clients did not decompress.
        let http = reqwest::Client::builder()
            .timeout(Self::CLIENT_TIMEOUT)
            .no_gzip()
            .no_deflate()
            .build()
            .expect("a plain HTTP client builds");
        Self::with_parts(
            path,
            http,
            Self::GITHUB_API,
            settings,
            Clock::system(),
            ReleaseVersion::running(),
        )
    }

    /// Every part given: tests point `api_base` at a mock server and fix the clock.
    pub fn with_parts(
        path: impl AsRef<Path>,
        http: reqwest::Client,
        api_base: &str,
        settings: Arc<SettingsStore>,
        clock: Clock,
        running: &str,
    ) -> Self {
        let path = path.as_ref().to_path_buf();
        let state = Self::load(&path);
        ReleaseCheck {
            path,
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            settings,
            clock,
            running: running.to_string(),
            gate: tokio::sync::Mutex::new(()),
            state: Mutex::new(state),
        }
    }

    /// The release this server runs, e.g. "2026.10.02".
    pub fn running(&self) -> &str {
        &self.running
    }

    pub fn view(&self) -> ReleaseCheckView {
        let settings = self.settings.current().updates.clone();
        let state = self.state.lock().clone();
        let repo = Self::repo(&settings);
        // An answer about another repo says nothing about this one.
        let releases = if state.repo.as_deref().is_some_and(|r| eq_ignore_case(r, &repo)) {
            state.releases
        } else {
            Vec::new()
        };
        let latest = releases.first().cloned();
        let running = ReleaseVersion::try_parse(self.running.as_str());
        let newer: Vec<ReleaseNote> = match running {
            Some(running) => releases
                .iter()
                .filter(|r| ReleaseVersion::try_parse(r.tag.as_str()).is_some_and(|v| v > running))
                .take(Self::SHOWN_NEWER)
                .cloned()
                .collect(),
            // A local build that names no release cannot be compared, so it is never told it is behind.
            None => Vec::new(),
        };
        let newest = latest
            .as_ref()
            .and_then(|l| ReleaseVersion::try_parse(l.tag.as_str()));
        let standing = match (running, newest) {
            (Some(running), Some(newest)) => {
                if !newer.is_empty() {
                    "behind"
                } else if running > newest {
                    "ahead"
                } else {
                    "current"
                }
            }
            _ => "unknown",
        };
        ReleaseCheckView {
            enabled: settings.check,
            repo,
            running: self.running.clone(),
            latest,
            update_available: !newer.is_empty(),
            newer,
            standing: standing.to_string(),
            checked_utc: state.checked_utc,
            error: state.error,
        }
    }

    /// Asks GitHub now, unless checks are off or a manual check ran a moment ago. Dropping the
    /// future cancels the check, which then saves nothing (the C# rethrew the cancellation).
    pub async fn check(&self, manual: bool) -> ReleaseCheckView {
        let settings = self.settings.current().updates.clone();
        if !settings.check {
            return self.view();
        }
        let _gate = self.gate.lock().await;
        let mut state = self.state.lock().clone();
        let now = self.clock.now();
        if manual
            && state
                .attempted_utc
                .is_some_and(|last| now - last < Self::MANUAL_SPACING)
        {
            return self.view();
        }

        let repo = Self::repo(&settings);
        if !state.repo.as_deref().is_some_and(|r| eq_ignore_case(r, &repo)) {
            state = ReleaseCheckState {
                repo: Some(repo.clone()),
                ..Default::default()
            };
        }
        state.attempted_utc = Some(now);

        if !REPO_PATTERN.is_match(&repo) {
            state.error = Some(format!(
                "Updates:Repo should read owner/name, like winters27/octo, not \"{repo}\"."
            ));
        } else {
            self.ask_git_hub(&repo, &mut state).await;
        }

        *self.state.lock() = state.clone();
        self.save(&state);
        self.view()
    }

    async fn ask_git_hub(&self, repo: &str, state: &mut ReleaseCheckState) {
        let outcome: anyhow::Result<()> = async {
            let mut request = self
                .http
                .get(format!("{}/repos/{repo}/releases?per_page=30", self.api_base))
                .header(
                    reqwest::header::USER_AGENT,
                    format!("Octo/{}", Self::sanitize(&self.running)),
                )
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            // A 304 answer does not count against GitHub's limit.
            if !state.releases.is_empty()
                && let Some(tag) = state.e_tag.as_deref().and_then(parse_entity_tag)
            {
                request = request.header(reqwest::header::IF_NONE_MATCH, tag);
            }

            let response = request.send().await?;
            if response.status() == StatusCode::NOT_MODIFIED {
                state.checked_utc = Some(self.clock.now());
                state.error = None;
                return Ok(());
            }
            if !response.status().is_success() {
                let refusal = Self::refusal(&response);
                info!("Release check for {repo}: {refusal}");
                state.error = Some(refusal);
                return Ok(());
            }

            let e_tag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_entity_tag);
            let body = response.bytes().await?;
            let document: Value = serde_json::from_slice(&body)?;
            state.releases = Self::server_releases(&document)?;
            state.e_tag = e_tag;
            state.checked_utc = Some(self.clock.now());
            state.error = None;
            Ok(())
        }
        .await;
        if let Err(e) = outcome {
            state.error = Some("Couldn't reach GitHub to look for a new release.".to_string());
            info!("Release check for {repo} failed: {e}");
        }
    }

    /// The dated, published, final releases in GitHub's answer, newest first.
    pub fn server_releases(root: &Value) -> anyhow::Result<Vec<ReleaseNote>> {
        let Some(list) = root.as_array() else {
            anyhow::bail!("GitHub's answer was not a list of releases.");
        };
        let mut found: Vec<(ReleaseVersion, ReleaseNote)> = Vec::new();
        for release in list {
            if json_bool(release, "draft") || json_bool(release, "prerelease") {
                continue;
            }
            let tag = json_text(release, "tag_name");
            let Some(version) = ReleaseVersion::try_parse(tag.unwrap_or_default()) else {
                continue;
            };
            let tag = tag.unwrap_or_default();
            if tag.contains('+') {
                continue;
            }
            let notes = truncate_utf16(json_text(release, "body").unwrap_or(""), Self::MAX_NOTES_LENGTH);
            // `TryGetDateTime` then `ToUniversalTime()`.
            let published = json_text(release, "published_at").and_then(datetime::parse_utc);
            let name = json_text(release, "name")
                .filter(|n| !n.is_empty())
                .unwrap_or(tag);
            found.push((
                version,
                ReleaseNote {
                    tag: tag.to_string(),
                    name: name.to_string(),
                    notes: notes.to_string(),
                    url: json_text(release, "html_url").unwrap_or("").to_string(),
                    published_utc: published,
                },
            ));
        }
        // OrderByDescending is stable.
        found.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
        Ok(found
            .into_iter()
            .map(|(_, note)| note)
            .take(Self::KEPT_RELEASES)
            .collect())
    }

    fn refusal(response: &reqwest::Response) -> String {
        let status = response.status();
        let limited = (status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS)
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|left| left == "0");
        if limited {
            return "GitHub's hourly limit for this address is used up. Octo tries again later.".to_string();
        }
        if status == StatusCode::NOT_FOUND {
            return "GitHub has no such repository. Check Updates:Repo.".to_string();
        }
        format!("GitHub answered {} when asked for releases.", status.as_u16())
    }

    /// The hosted service's loop, as a worker: waits 20 s for the server to finish starting,
    /// then checks whenever one is due (or the repo changed), looking every 5 minutes so turning
    /// checks on or changing the repo is noticed soon.
    pub async fn run(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        // Let the server finish starting before it talks to anyone.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(20)) => {}
            _ = token.cancelled() => return Ok(()),
        }
        while !token.is_cancelled() {
            let (attempted, state_repo) = {
                let state = self.state.lock();
                (state.attempted_utc, state.repo.clone())
            };
            let due = attempted.unwrap_or_else(datetime::min_value) + Self::INTERVAL;
            let settings = self.settings.current().updates.clone();
            let repo_changed = !state_repo
                .as_deref()
                .is_some_and(|r| eq_ignore_case(r, &Self::repo(&settings)));
            if settings.check && (self.clock.now() >= due || repo_changed) {
                tokio::select! {
                    _ = self.check(false) => {}
                    _ = token.cancelled() => return Ok(()),
                }
            }
            // Woken every few minutes, so turning checks on or changing the repo is noticed soon.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5 * 60)) => {}
                _ = token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    fn repo(settings: &UpdateSettings) -> String {
        if settings.repo.trim().is_empty() {
            "winters27/octo".to_string()
        } else {
            settings.repo.trim().trim_matches('/').to_string()
        }
    }

    /// A product version token may hold only token characters.
    fn sanitize(version: &str) -> String {
        let clean = NOT_TOKEN.replace_all(version, "");
        if clean.is_empty() {
            "unknown".to_string()
        } else {
            clean.into_owned()
        }
    }

    fn load(path: &Path) -> ReleaseCheckState {
        let loaded = (|| -> anyhow::Result<ReleaseCheckState> {
            let Some(text) = state_file::read_text(path)? else {
                return Ok(ReleaseCheckState::default());
            };
            Ok(serde_json::from_str::<Option<ReleaseCheckState>>(&text)?.unwrap_or_default())
        })();
        loaded.unwrap_or_else(|e| {
            warn!(
                "Could not read {}; the next check starts fresh: {e}",
                path.display()
            );
            ReleaseCheckState::default()
        })
    }

    fn save(&self, state: &ReleaseCheckState) {
        if let Err(e) = state_file::save_atomic(&self.path, &octo_core::json::to_string(state)) {
            warn!("Could not save {}: {e}", self.path.display());
        }
    }
}

/// `element.TryGetProperty(name, out v) && v.ValueKind == True`.
fn json_bool(element: &Value, name: &str) -> bool {
    matches!(element.get(name), Some(Value::Bool(true)))
}

/// The property when it is a string.
fn json_text<'a>(element: &'a Value, name: &str) -> Option<&'a str> {
    element.get(name).and_then(Value::as_str)
}

/// The first `max` UTF-16 code units (`notes[..max]`). A surrogate pair the cut would split
/// is left out whole, where .NET kept its first half.
fn truncate_utf16(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..i];
        }
    }
    text
}

/// `EntityTagHeaderValue.TryParse` + `ToString()`: an optionally weak (`W/`) quoted string,
/// surrounding whitespace ignored; None when it is not one.
fn parse_entity_tag(text: &str) -> Option<String> {
    let text = text.trim();
    let (weak, rest) = match text.strip_prefix("W/") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('"') {
        return None;
    }
    Some(if weak {
        format!("W/\"{inner}\"")
    } else {
        format!("\"{inner}\"")
    })
}

#[cfg(test)]
#[path = "release_check_tests.rs"]
mod tests;
