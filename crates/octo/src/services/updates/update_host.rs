//! Port of `Services/Updates/UpdateHost.cs`: the `config/update/` files shared with the host
//! helper (`scripts/updater/octo-updater.sh`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::Clock;
use octo_core::json::datetime::{min_value, parse_utc};
use octo_core::updates::ReleaseVersion;
use parking_lot::Mutex;
use regex::Regex;
use tracing::{debug, warn};

use crate::services::state_file;

/// The host helper, as it describes itself in config/update/helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateHelperInfo {
    pub version: String,
    pub mode: String,
    pub dir: Option<String>,
    pub installed_utc: Option<DateTime<Utc>>,
}

/// One update run, as the host helper reports it in config/update/status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRunStatus {
    pub id: String,
    pub tag: Option<String>,
    pub from: Option<String>,
    pub state: String,
    pub step: Option<String>,
    pub error: Option<String>,
    pub started_utc: Option<DateTime<Utc>>,
    pub finished_utc: Option<DateTime<Utc>>,
}

/// The states the helper writes, in the order a run passes through them.
pub struct UpdateRunStates;

impl UpdateRunStates {
    pub const ACCEPTED: &'static str = "accepted";
    pub const FETCHING: &'static str = "fetching";
    pub const BUILDING: &'static str = "building";
    pub const RESTARTING: &'static str = "restarting";
    pub const DONE: &'static str = "done";
    pub const FAILED: &'static str = "failed";

    pub fn running(state: &str) -> bool {
        matches!(
            state,
            Self::ACCEPTED | Self::FETCHING | Self::BUILDING | Self::RESTARTING
        )
    }
}

/// Why [`UpdateHost::request`] wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum UpdateRequestError {
    /// `ArgumentException`: the tag names no release.
    #[error("\"{0}\" is not a release. (Parameter 'tag')")]
    NotARelease(String),
    /// The request file could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

static SAFE_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._@-]{1,64}$").expect("a fixed pattern compiles"));

/// The exchange with the host helper, through files in config/update/. Octo writes a request; a
/// small systemd service on the host (scripts/updater) picks it up, updates and restarts Octo,
/// and writes its progress back. Octo never touches Docker itself.
///
/// Every file is plain key=value lines, so the helper needs no JSON tool, and each is written to
/// a temporary name and renamed, so neither side ever reads half a file.
pub struct UpdateHost {
    dir: PathBuf,
    clock: Clock,
    lock: Mutex<()>,
    unanswered: Mutex<Option<String>>,
}

impl UpdateHost {
    /// How long a request may wait for the helper before Octo gives up on it.
    pub const ANSWER_TIMEOUT: TimeDelta = TimeDelta::seconds(90);

    /// A run that has said nothing for this long has stopped, whatever its last state.
    pub const RUN_TIMEOUT: TimeDelta = TimeDelta::minutes(40);

    pub fn new(dir: impl Into<PathBuf>, clock: Clock) -> Self {
        Self {
            dir: dir.into(),
            clock,
            lock: Mutex::new(()),
            unanswered: Mutex::new(None),
        }
    }

    fn request_path(&self) -> PathBuf {
        self.dir.join("request")
    }

    fn helper_path(&self) -> PathBuf {
        self.dir.join("helper")
    }

    fn status_path(&self) -> PathBuf {
        self.dir.join("status")
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("log")
    }

    /// The id of the last request the helper never answered, so the page can say so.
    pub fn unanswered(&self) -> Option<String> {
        self.unanswered.lock().clone()
    }

    pub fn helper(&self) -> Option<UpdateHelperInfo> {
        let values = read(&self.helper_path())?;
        let mode = if values.get("mode").map(String::as_str) == Some("image") {
            "image"
        } else {
            "build"
        };
        Some(UpdateHelperInfo {
            version: values.get("version").cloned().unwrap_or_else(|| "1".to_string()),
            mode: mode.to_string(),
            dir: values.get("dir").filter(|dir| !dir.is_empty()).cloned(),
            installed_utc: time(values.get("installed")),
        })
    }

    pub fn status(&self) -> Option<UpdateRunStatus> {
        let values = read(&self.status_path())?;
        let id = values.get("id").filter(|id| !id.is_empty())?.clone();
        Some(UpdateRunStatus {
            id,
            tag: values.get("tag").cloned(),
            from: values.get("from").cloned(),
            state: values
                .get("state")
                .cloned()
                .unwrap_or_else(|| UpdateRunStates::FAILED.to_string()),
            step: values.get("step").cloned(),
            error: values.get("error").cloned(),
            started_utc: time(values.get("started")),
            finished_utc: time(values.get("finished")),
        })
    }

    /// The last lines of the helper's log, for a run that failed. (C#'s default is 40.)
    pub fn log_tail(&self, lines: usize) -> Vec<String> {
        let path = self.log_path();
        if !path.exists() {
            return Vec::new();
        }
        match state_file::read_text(&path) {
            Ok(text) => {
                let all = state_file::lines(&text);
                all[all.len().saturating_sub(lines)..]
                    .iter()
                    .map(|line| line.to_string())
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    }

    /// The id of a request still waiting for the helper, after dropping one it has ignored too long.
    pub fn pending(&self) -> Option<String> {
        let _guard = self.lock.lock();
        let path = self.request_path();
        let values = read(&path)?;
        let id = values.get("id").filter(|id| !id.is_empty())?.clone();
        if self.status().is_some_and(|status| status.id == id) {
            return None;
        }
        let at = time(values.get("at")).unwrap_or_else(|| last_write_time_utc(&path));
        if self.clock.now() - at < Self::ANSWER_TIMEOUT {
            return Some(id);
        }
        // Nobody picked it up. It is removed so a helper installed later never runs a stale request.
        let _ = std::fs::remove_file(&path);
        warn!("The update request {id} was never picked up by the host helper; dropped it");
        *self.unanswered.lock() = Some(id);
        None
    }

    /// Whether a run is under way: a request waiting, or the helper still working on one.
    pub fn busy(&self) -> bool {
        if self.pending().is_some() {
            return true;
        }
        self.status().is_some_and(|status| {
            UpdateRunStates::running(&status.state)
                && self.clock.now() - status.started_utc.unwrap_or_else(min_value) < Self::RUN_TIMEOUT
        })
    }

    /// Writes a request for the helper and returns its id.
    pub fn request(&self, tag: &str, requested_by: &str) -> Result<String, UpdateRequestError> {
        if ReleaseVersion::try_parse(tag).is_none() || tag.contains('+') {
            return Err(UpdateRequestError::NotARelease(tag.to_string()));
        }
        let _guard = self.lock.lock();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let by = if SAFE_WORD.is_match(requested_by) {
            requested_by
        } else {
            "dashboard"
        };
        let at = self.clock.now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        write(
            &self.request_path(),
            &[("id", id.as_str()), ("tag", tag), ("by", by), ("at", at.as_str())],
        )?;
        *self.unanswered.lock() = None;
        Ok(id)
    }

    /// The key=value lines: split on the first '=', both sides trimmed, lines without a key
    /// skipped, a later key replacing an earlier one.
    pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
        let mut values = HashMap::new();
        for line in lines {
            let Some(split) = line.find('=').filter(|&split| split > 0) else {
                continue;
            };
            values.insert(
                line[..split].trim().to_string(),
                line[split + 1..].trim().to_string(),
            );
        }
        values
    }
}

fn read(path: &Path) -> Option<HashMap<String, String>> {
    if !path.exists() {
        return None;
    }
    match state_file::read_text(path) {
        Ok(text) => Some(UpdateHost::parse(state_file::lines(&text))),
        Err(error) => {
            debug!("Could not read {}: {error}", path.display());
            None
        }
    }
}

fn write(path: &Path, values: &[(&str, &str)]) -> std::io::Result<()> {
    let text: String = values
        .iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect();
    state_file::write_atomic(path, text.as_bytes())
}

/// `DateTime.TryParse` with `AdjustToUniversal | AssumeUniversal`, for the ISO 8601 times the
/// helper writes.
fn time(text: Option<&String>) -> Option<DateTime<Utc>> {
    text.filter(|text| !text.trim().is_empty())
        .and_then(|text| parse_utc(text))
}

/// `File.GetLastWriteTimeUtc`: 1601-01-01 for a file that is not there.
fn last_write_time_utc(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| {
            DateTime::parse_from_rfc3339("1601-01-01T00:00:00Z")
                .map(|time| time.with_timezone(&Utc))
                .unwrap_or_else(|_| min_value())
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;

    use super::*;

    /// The files that hand Update now to the host helper. Octo can only ask, by writing a
    /// request naming a release; the helper writes back what it did. A request nobody picks up
    /// is dropped, so a helper installed later never runs it by surprise.
    struct Fixture {
        _dir: tempfile::TempDir,
        dir: PathBuf,
        now: Arc<Mutex<DateTime<Utc>>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            // The C# test used a folder that did not exist yet.
            let path = dir.path().join("update");
            Self {
                _dir: dir,
                dir: path,
                now: Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap())),
            }
        }

        fn host(&self) -> UpdateHost {
            let now = self.now.clone();
            UpdateHost::new(&self.dir, Clock::new(move || *now.lock()))
        }

        fn advance(&self, seconds: i64) {
            *self.now.lock() += TimeDelta::seconds(seconds);
        }

        fn write_file(&self, name: &str, lines: &[&str]) {
            std::fs::create_dir_all(&self.dir).expect("created");
            // File.WriteAllLines: each line ends with a newline.
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(self.dir.join(name), text).expect("written");
        }

        fn lines(&self, name: &str) -> Vec<String> {
            std::fs::read_to_string(self.dir.join(name))
                .expect("the file is there")
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn exists(&self, name: &str) -> bool {
            self.dir.join(name).exists()
        }
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn a_request_names_the_release_and_who_asked() {
        let fixture = Fixture::new();
        let id = fixture
            .host()
            .request("2026.10.04", "winters")
            .expect("requested");

        let lines = fixture.lines("request");
        assert!(lines.contains(&format!("id={id}")));
        assert!(lines.contains(&"tag=2026.10.04".to_string()));
        assert!(lines.contains(&"by=winters".to_string()));
        assert!(lines.contains(&"at=2026-10-03T12:00:00Z".to_string()));
        assert!(!fixture.exists("request.tmp"));
    }

    #[test]
    fn only_a_release_can_be_requested() {
        for tag in ["2026.10.04; rm -rf /", "main", "desktop-v1.3.2", "2026.10.04+abc"] {
            let fixture = Fixture::new();
            assert!(
                matches!(
                    fixture.host().request(tag, "winters"),
                    Err(UpdateRequestError::NotARelease(_))
                ),
                "{tag}"
            );
            assert!(!fixture.exists("request"), "{tag}");
        }
    }

    #[test]
    fn an_odd_user_name_is_not_written_into_the_request() {
        let fixture = Fixture::new();
        fixture
            .host()
            .request("2026.10.04", "bad\nstate=done")
            .expect("requested");

        let lines = fixture.lines("request");
        assert!(lines.contains(&"by=dashboard".to_string()));
        assert!(!lines.iter().any(|line| line.starts_with("state=")));
    }

    #[test]
    fn a_request_waits_for_the_helper_and_is_dropped_when_nobody_answers() {
        let fixture = Fixture::new();
        let host = fixture.host();
        let id = host.request("2026.10.04", "winters").expect("requested");

        fixture.advance(89);
        assert_eq!(host.pending(), Some(id.clone()));
        assert!(host.busy());

        fixture.advance(2);
        assert_eq!(host.pending(), None);
        assert_eq!(host.unanswered(), Some(id));
        assert!(!fixture.exists("request"));
        assert!(!host.busy());
    }

    #[test]
    fn a_request_the_helper_answered_is_no_longer_pending() {
        let fixture = Fixture::new();
        let host = fixture.host();
        let id = host.request("2026.10.04", "winters").expect("requested");
        fixture.write_file(
            "status",
            &[
                &format!("id={id}"),
                "tag=2026.10.04",
                "state=accepted",
                "started=2026-10-03T12:00:01Z",
            ],
        );

        assert_eq!(host.pending(), None);
        assert!(host.busy());
        assert_eq!(host.unanswered(), None);
    }

    #[test]
    fn a_run_that_stopped_reporting_is_not_busy_forever() {
        let fixture = Fixture::new();
        fixture.write_file(
            "status",
            &[
                "id=a",
                "tag=2026.10.04",
                "state=building",
                "started=2026-10-03T11:00:00Z",
            ],
        );

        assert!(!fixture.host().busy());
    }

    #[test]
    fn a_finished_run_is_not_busy() {
        let fixture = Fixture::new();
        fixture.write_file(
            "status",
            &[
                "id=a",
                "tag=2026.10.04",
                "state=done",
                "started=2026-10-03T11:59:00Z",
                "finished=2026-10-03T11:59:50Z",
            ],
        );

        assert!(!fixture.host().busy());
    }

    #[test]
    fn the_helper_describes_itself() {
        let fixture = Fixture::new();
        assert!(fixture.host().helper().is_none());

        fixture.write_file(
            "helper",
            &[
                "version=1",
                "mode=image",
                "dir=/opt/octo",
                "installed=2026-10-03T10:00:00Z",
            ],
        );
        let helper = fixture.host().helper().expect("installed");

        assert_eq!(helper.mode, "image");
        assert_eq!(helper.dir.as_deref(), Some("/opt/octo"));
        assert_eq!(helper.installed_utc, Some(utc(2026, 10, 3, 10, 0, 0)));
    }

    #[test]
    fn an_unknown_mode_is_the_built_from_source_install() {
        let fixture = Fixture::new();
        fixture.write_file("helper", &["version=1", "mode=something"]);

        assert_eq!(fixture.host().helper().expect("installed").mode, "build");
    }

    #[test]
    fn the_status_reads_back_with_its_times() {
        let fixture = Fixture::new();
        fixture.write_file(
            "status",
            &[
                "id=a",
                "tag=2026.10.04",
                "from=2026.10.01",
                "state=failed",
                "step=Building Octo 2026.10.04",
                "error=The build failed, so nothing was restarted.",
                "started=2026-10-03T11:59:00Z",
                "finished=2026-10-03T12:01:00Z",
            ],
        );

        let status = fixture.host().status().expect("a status");

        assert_eq!(status.state, "failed");
        assert_eq!(status.from.as_deref(), Some("2026.10.01"));
        assert_eq!(
            status.error.as_deref(),
            Some("The build failed, so nothing was restarted.")
        );
        assert_eq!(status.finished_utc, Some(utc(2026, 10, 3, 12, 1, 0)));
    }

    #[test]
    fn lines_without_a_key_are_ignored_and_values_may_hold_equals() {
        let values = UpdateHost::parse(["junk", "=nokey", "error=a=b", " tag = 2026.10.04 "]);

        assert_eq!(values["error"], "a=b");
        assert_eq!(values["tag"], "2026.10.04");
        assert_eq!(values.len(), 2);
    }

    // ---- Rust-only ------------------------------------------------------------------------

    /// The fixtures the helper wrote (state-files.md §5) read as the C# read them, and a
    /// request is written in exactly the fixture's shape.
    #[test]
    fn the_update_fixtures_read_and_a_request_is_written_in_their_shape() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rust-migration/fixtures/state/update");
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.dir).expect("created");
        for name in ["status", "helper", "log"] {
            std::fs::copy(fixtures.join(name), fixture.dir.join(name)).expect("copied");
        }
        let host = fixture.host();

        let status = host.status().expect("a status");
        assert_eq!(status.id, "95d3ba8f-b736-4adf-8904-4bf1b4376920");
        assert_eq!(status.state, UpdateRunStates::BUILDING);
        assert_eq!(status.error.as_deref(), Some(""));
        assert_eq!(status.started_utc, Some(utc(2026, 10, 3, 18, 5, 3)));
        assert_eq!(status.finished_utc, None);

        let helper = host.helper().expect("installed");
        assert_eq!(
            helper,
            UpdateHelperInfo {
                version: "1".into(),
                mode: "build".into(),
                dir: Some("/opt/octo".into()),
                installed_utc: Some(utc(2026, 9, 30, 12, 0, 0)),
            }
        );

        let log = host.log_tail(40);
        assert_eq!(log.len(), 2);
        assert!(log[1].ends_with("fetch --tags --force origin"));

        *fixture.now.lock() = utc(2026, 10, 3, 18, 5, 0);
        let id = host.request("2026.10.03.2", "brandon").expect("requested");
        let expected = std::fs::read_to_string(fixtures.join("request"))
            .expect("the fixture")
            .replace("95d3ba8f-b736-4adf-8904-4bf1b4376920", &id);
        assert_eq!(
            std::fs::read_to_string(fixture.dir.join("request")).expect("written"),
            expected
        );
    }

    #[test]
    fn the_log_tail_keeps_the_last_lines() {
        let fixture = Fixture::new();
        let lines: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
        fixture.write_file("log", &lines.iter().map(String::as_str).collect::<Vec<_>>());

        let tail = fixture.host().log_tail(40);
        assert_eq!(tail.len(), 40);
        assert_eq!(tail.last().map(String::as_str), Some("line 60"));
        assert_eq!(tail[0], "line 21");
    }

    #[test]
    fn a_request_without_a_time_waits_from_when_it_was_written() {
        let fixture = Fixture::new();
        fixture.write_file("request", &["id=x", "tag=2026.10.04"]);
        // Without an `at`, the request's age is counted from the file's last write.
        *fixture.now.lock() = Utc::now();
        assert_eq!(fixture.host().pending(), Some("x".to_string()));
    }
}
