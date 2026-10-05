//! Port of `Services/Library/QualityUpgradeWorker.cs`: the weekly quality upgrade (#70), its
//! memory (`<config>/quality-upgrade.json`, state-files.md §4.15), and the Navidrome song-list
//! parser `NavidromePlaylistApi::list_songs` uses.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::common::dotnet;
use octo_core::json::datetime;
use octo_core::settings::{LibraryAction, LibraryActionSettings, SettingsStore};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::duplicate_scan_worker::is_lossless_file;
use super::library_action_executor::{LibraryActionExecutor, LibraryActionRequest};
use super::upgrade_queue::{ApplyFn, OfflineFn, apply_with, soulseek_offline_with};
use super::{LibraryActionOutcome, LibraryActionState, NavidromePlaylistApi, UpgradeQueue, UpgradeSources};
use crate::services::common::IAcquisitionActivity;
use crate::services::framework::DotnetDictionary;
use crate::services::soulseek::ISoulseekLink;
use crate::services::state_file;
use crate::services::subsonic::NavidromeIdentityService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySongRow {
    pub id: String,
    pub path: String,
    pub library_path: Option<String>,
    pub size: i64,
    pub suffix: String,
    pub bit_rate: i32,
    pub title: String,
    pub artist: String,
    pub duration: Option<i32>,
    pub album: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QualityUpgradeAttempt {
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub at_utc: DateTime<Utc>,
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub outcome: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QualityUpgradeState {
    #[serde(default, with = "datetime::utc_option")]
    pub last_run_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_outcome: Option<String>,
    #[serde(default, with = "super::state_dictionary")]
    pub attempts: DotnetDictionary<QualityUpgradeAttempt>,
}

/// What the Better quality page shows of the weekly run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityUpgradeStatus {
    pub per_week: i32,
    pub off: Option<String>,
    pub last_run_utc: Option<DateTime<Utc>>,
    pub last_outcome: Option<String>,
    pub next_due_utc: Option<DateTime<Utc>>,
    pub tried: usize,
}

/// What the weekly upgrade has tried, by file. Written straight away on every change: there is
/// about one a day, and a restart must not forget a run and start a second one early.
pub struct QualityUpgradeStore {
    path: Option<PathBuf>,
    state: Mutex<QualityUpgradeState>,
}

impl Default for QualityUpgradeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityUpgradeStore {
    /// A store in memory only (`new QualityUpgradeStore()`).
    pub fn new() -> Self {
        Self::with_path(None)
    }

    /// A store kept in `path`, read now. A file that cannot be read is logged and the store
    /// starts empty.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let path = path.filter(|p| !dotnet::is_blank(&p.to_string_lossy()));
        let state = match &path {
            Some(path) => Self::load(path).unwrap_or_else(|e| {
                warn!("quality upgrade state could not be read: {e}");
                QualityUpgradeState::default()
            }),
            None => QualityUpgradeState::default(),
        };
        QualityUpgradeStore {
            path,
            state: Mutex::new(state),
        }
    }

    fn load(path: &Path) -> anyhow::Result<QualityUpgradeState> {
        let Some(text) = state_file::read_text(path)? else {
            return Ok(QualityUpgradeState::default());
        };
        Ok(serde_json::from_str::<Option<QualityUpgradeState>>(&text)?.unwrap_or_default())
    }

    pub fn snapshot(&self) -> QualityUpgradeState {
        self.state.lock().clone()
    }

    /// Changes the state and writes it. Serialized inside the lock, written outside it.
    pub fn update(&self, change: impl FnOnce(&mut QualityUpgradeState)) {
        let json = {
            let mut state = self.state.lock();
            change(&mut state);
            octo_core::json::to_string(&*state)
        };
        let Some(path) = &self.path else {
            return;
        };
        if let Err(e) = state_file::save_atomic(path, &json) {
            warn!("quality upgrade state could not be written: {e}");
        }
    }
}

/// One walk of the library: the songs, and whether the walk reached the end.
pub type ListSongsFn = Arc<dyn Fn() -> BoxFuture<'static, (Vec<LibrarySongRow>, bool)> + Send + Sync>;

/// The seams the C# exposed as settable properties, "the same way SoulseekClient exposes Clock
/// and PollInterval".
#[derive(Clone)]
pub struct QualityUpgradeSeams {
    pub clock: Clock,
    pub acquisitions_idle: Arc<dyn Fn() -> bool + Send + Sync>,
    pub has_admin_identity: Arc<dyn Fn() -> bool + Send + Sync>,
    pub apply: ApplyFn,
    pub list_songs: ListSongsFn,
    pub soulseek_offline: OfflineFn,
    pub source_ready: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl QualityUpgradeSeams {
    /// Seams that do nothing: idle, an admin, no songs, Soulseek up, a source ready, and an
    /// apply that fails. Tests set the ones they drive.
    pub fn idle() -> Self {
        QualityUpgradeSeams {
            clock: Clock::system(),
            acquisitions_idle: Arc::new(|| true),
            has_admin_identity: Arc::new(|| true),
            apply: Arc::new(|_| {
                async { Ok(LibraryActionOutcome::new(LibraryActionState::Failed, None)) }.boxed()
            }),
            list_songs: Arc::new(|| async { (Vec::new(), true) }.boxed()),
            soulseek_offline: Arc::new(|| async { false }.boxed()),
            source_ready: Arc::new(|| true),
        }
    }
}

/// What a tick did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Off,
    NotDue,
    Busy,
    Offline,
    Unreachable,
    NothingToDo,
    Ran,
}

/// Trickles lossy songs through Better quality, a few a week (#70). Every safety the action has
/// applies unchanged, because this only ever calls it: the allowlist, dry run, the quarantine,
/// and putting the original back when the new file is not really better, and the upgraded song
/// keeps its place in Navidrome, with its plays, favorites and playlist entries (W8).
pub struct QualityUpgradeWorker {
    store: Arc<QualityUpgradeStore>,
    settings: Arc<SettingsStore>,
    upgrades: Option<Arc<UpgradeQueue>>,
    last_unreachable_warning: Mutex<Option<DateTime<Utc>>>,
    seams: Mutex<QualityUpgradeSeams>,
}

impl QualityUpgradeWorker {
    pub const PAGE_SIZE: i32 = 1000;
    pub const MAX_PAGES: i32 = 200;
    /// A song that found nothing is not looked for again for four weeks, so a small library
    /// does not search for the same missing song every few hours.
    pub const RETRY_AFTER: TimeDelta = TimeDelta::days(28);
    const FIRST_CHECK: Duration = Duration::from_secs(5 * 60);
    const CHECK_INTERVAL: Duration = Duration::from_secs(60);
    const UNREACHABLE_WARNING_EVERY: TimeDelta = TimeDelta::hours(1);

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<QualityUpgradeStore>,
        executor: Arc<LibraryActionExecutor>,
        activity: Arc<dyn IAcquisitionActivity>,
        navidrome: Arc<NavidromePlaylistApi>,
        identity: NavidromeIdentityService,
        settings: Arc<SettingsStore>,
        soulseek: Option<Arc<dyn ISoulseekLink>>,
        upgrades: Option<Arc<UpgradeQueue>>,
        sources: Option<Arc<UpgradeSources>>,
    ) -> Self {
        let ready_sources = sources.clone();
        let seams = QualityUpgradeSeams {
            clock: Clock::system(),
            acquisitions_idle: Arc::new(move || !activity.is_busy()),
            has_admin_identity: Arc::new(move || identity.has_admin_identity()),
            apply: apply_with(executor),
            list_songs: Arc::new(move || {
                let navidrome = navidrome.clone();
                async move { Self::walk(&navidrome).await }.boxed()
            }),
            // Out only when every source is: a Soulseek outage leaves Lidarr when it is set up.
            soulseek_offline: soulseek_offline_with(sources, soulseek),
            source_ready: Arc::new(move || ready_sources.as_ref().is_none_or(|s| s.ready())),
        };
        Self::with_seams(store, settings, upgrades, seams)
    }

    /// A worker over the given seams (the C# tests' object initializer).
    pub fn with_seams(
        store: Arc<QualityUpgradeStore>,
        settings: Arc<SettingsStore>,
        upgrades: Option<Arc<UpgradeQueue>>,
        seams: QualityUpgradeSeams,
    ) -> Self {
        QualityUpgradeWorker {
            store,
            settings,
            upgrades,
            last_unreachable_warning: Mutex::new(None),
            seams: Mutex::new(seams),
        }
    }

    /// Changes a seam after construction, as the C# tests set a property.
    pub fn configure(&self, change: impl FnOnce(&mut QualityUpgradeSeams)) {
        change(&mut self.seams.lock());
    }

    fn seams(&self) -> QualityUpgradeSeams {
        self.seams.lock().clone()
    }

    /// A week spread over `per_week` runs; None when it is off.
    pub fn interval(per_week: i32) -> Option<TimeDelta> {
        if per_week <= 0 {
            return None;
        }
        const WEEK_TICKS: i64 = 7 * 24 * 3600 * 10_000_000;
        Some(TimeDelta::nanoseconds(
            WEEK_TICKS / i64::from(per_week.clamp(1, 500)) * 100,
        ))
    }

    pub fn acting_user(settings: &LibraryActionSettings) -> Option<String> {
        settings
            .allowed_users
            .iter()
            .map(|user| user.trim())
            .find(|user| !user.is_empty())
            .map(str::to_string)
    }

    pub fn why_off(
        settings: &LibraryActionSettings,
        has_admin: bool,
        source_ready: bool,
    ) -> Option<&'static str> {
        if settings.effective_upgrade_per_week() <= 0 {
            Some("Off.")
        }
        // Every try would fail and be stamped as tried for four weeks.
        else if !source_ready {
            Some("Better quality has no source set up: it needs slskd or Lidarr.")
        } else if !settings.enabled {
            Some("Library actions are off.")
        } else if !settings
            .effective_actions()
            .iter()
            .any(|a| a.action == LibraryAction::BetterQuality && a.enabled)
        {
            Some("Better quality is not switched on.")
        } else if Self::acting_user(settings).is_none() {
            Some("Nobody is on the allowed list.")
        } else if !has_admin {
            Some("Octo needs a Navidrome admin credential to read the library.")
        } else {
            None
        }
    }

    /// The library path and size, never the Navidrome id: ids change when a file moves, and
    /// Navidrome 0.64 changed all of them. A failed upgrade puts the original back unchanged, so
    /// its key holds. A successful one turns it into a FLAC, which is never picked.
    pub fn key_of(song: &LibrarySongRow) -> String {
        let mut path = song.path.replace('\\', "/");
        let root = song.library_path.as_deref().unwrap_or("").replace('\\', "/");
        let root = root.trim_end_matches('/');
        if !root.is_empty() && path.starts_with(&format!("{root}/")) {
            path = path[root.len() + 1..].to_string();
        }
        format!("{}|{}", path.trim_start_matches('/'), song.size)
    }

    /// Never tried first, then the longest ago. A rehearsal counts as a try only while dry run
    /// is on, so turning dry run off starts again from the top.
    pub fn pick<'a>(
        songs: &'a [LibrarySongRow],
        state: &QualityUpgradeState,
        now: DateTime<Utc>,
        dry_run: bool,
    ) -> Option<&'a LibrarySongRow> {
        let mut candidates: Vec<(&LibrarySongRow, Option<&QualityUpgradeAttempt>, String)> = songs
            .iter()
            .filter(|song| !is_lossless_file(&song.suffix, song.bit_rate))
            .map(|song| {
                let key = Self::key_of(song);
                let tried = state
                    .attempts
                    .get(&key)
                    .filter(|a| dry_run || a.outcome != LibraryActionState::Rehearsed.name());
                (song, tried, key)
            })
            .filter(|(_, tried, _)| tried.is_none_or(|a| now - a.at_utc >= Self::RETRY_AFTER))
            .collect();
        candidates.sort_by(|a, b| {
            a.1.is_some()
                .cmp(&b.1.is_some())
                .then_with(|| {
                    let at =
                        |t: &Option<&QualityUpgradeAttempt>| t.map_or(datetime::min_value(), |a| a.at_utc);
                    at(&a.1).cmp(&at(&b.1))
                })
                .then_with(|| compare_ordinal(&a.2, &b.2))
        });
        candidates.first().map(|(song, _, _)| *song)
    }

    /// One minute's check: off, not due, busy, Soulseek out, Navidrome silent, nothing left, or
    /// one song tried.
    pub async fn tick(&self) -> Tick {
        let settings = self.settings.current().library_actions.clone();
        let seams = self.seams();
        if Self::why_off(&settings, (seams.has_admin_identity)(), (seams.source_ready)()).is_some() {
            return Tick::Off;
        }
        let now = seams.clock.now();
        let state = self.store.snapshot();
        if let (Some(last), Some(interval)) = (
            state.last_run_utc,
            Self::interval(settings.effective_upgrade_per_week()),
        ) && now - last < interval
        {
            return Tick::NotDue;
        }
        // Never queue ahead of a person: a heart, star or play in flight means try again next minute.
        if !(seams.acquisitions_idle)() {
            return Tick::Busy;
        }
        // Songs someone asked to upgrade go first, and the weekly run never competes with them.
        if self.upgrades.as_ref().is_some_and(|u| u.open_count() > 0) {
            return Tick::Busy;
        }
        // An upgrade during a Soulseek outage finds nothing and would not look at that song again
        // for four weeks. Not stamped, so the run happens once slskd is back.
        if (seams.soulseek_offline)().await {
            return Tick::Offline;
        }

        let (songs, complete) = (seams.list_songs)().await;
        if !complete && songs.is_empty() {
            // Navidrome did not answer. Stamping the run would spend the week's slot on a
            // library nobody could read, so it is tried again next minute instead.
            let mut warned = self.last_unreachable_warning.lock();
            if warned.is_none_or(|at| now - at >= Self::UNREACHABLE_WARNING_EVERY) {
                *warned = Some(now);
                warn!("Quality upgrade: Navidrome did not list the library; trying again every minute");
            } else {
                debug!("Quality upgrade: Navidrome still did not list the library");
            }
            return Tick::Unreachable;
        }
        let pick = Self::pick(&songs, &state, now, settings.dry_run).cloned();
        self.store.update(|s| {
            // Stamped before the attempt, so a crash or restart mid-download cannot cause a burst.
            s.last_run_utc = Some(now);
            if complete {
                let present: std::collections::HashSet<String> = songs.iter().map(Self::key_of).collect();
                let gone: Vec<String> = s
                    .attempts
                    .keys()
                    .filter(|k| !present.contains(*k))
                    .cloned()
                    .collect();
                for key in gone {
                    s.attempts.remove(&key);
                }
            }
            if pick.is_none() {
                s.last_outcome = Some("Nothing".to_string());
            }
        });
        let Some(pick) = pick else {
            return Tick::NothingToDo;
        };

        info!(
            "Quality upgrade: trying '{} - {}' ({})",
            pick.artist, pick.title, pick.suffix
        );
        let user = Self::acting_user(&settings).unwrap_or_default();
        let outcome = match (seams.apply)(LibraryActionRequest::new(
            LibraryAction::BetterQuality,
            &pick.id,
            user,
        ))
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => LibraryActionOutcome::new(LibraryActionState::Failed, Some(e.to_string())),
        };

        let key = Self::key_of(&pick);
        let at = seams.clock.now();
        self.store.update(|s| {
            s.attempts.set(
                key,
                QualityUpgradeAttempt {
                    at_utc: at,
                    outcome: outcome.state.name().to_string(),
                    detail: outcome.detail.clone(),
                },
            );
            s.last_outcome = Some(outcome.state.name().to_string());
        });
        Tick::Ran
    }

    /// `ListSongs`: one walk of the library, and whether it reached the end. The Better quality
    /// page's list (`GET /api/admin/lossy`) reads it too.
    pub async fn list_songs(&self) -> (Vec<LibrarySongRow>, bool) {
        let list = self.seams().list_songs;
        list().await
    }

    /// What the weekly run has tried, by file, for the Better quality page.
    pub fn tried(&self) -> QualityUpgradeState {
        self.store.snapshot()
    }

    pub fn status(&self) -> QualityUpgradeStatus {
        let settings = self.settings.current().library_actions.clone();
        let seams = self.seams();
        let state = self.store.snapshot();
        let off = Self::why_off(&settings, (seams.has_admin_identity)(), (seams.source_ready)());
        let next = match (off, Self::interval(settings.effective_upgrade_per_week())) {
            (None, Some(interval)) => Some(
                state
                    .last_run_utc
                    .map_or_else(|| seams.clock.now(), |last| last + interval),
            ),
            _ => None,
        };
        QualityUpgradeStatus {
            per_week: settings.effective_upgrade_per_week(),
            off: off.map(str::to_string),
            last_run_utc: state.last_run_utc,
            last_outcome: state.last_outcome.clone(),
            next_due_utc: next,
            tried: state.attempts.len(),
        }
    }

    /// `ExecuteAsync`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut wait = Self::FIRST_CHECK;
        while !stopping.is_cancelled() {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stopping.cancelled() => break,
            }
            wait = Self::CHECK_INTERVAL;
            // Per-tick catch: BackgroundServiceExceptionBehavior defaults to StopHost.
            let tick = tokio::select! {
                tick = std::panic::AssertUnwindSafe(self.tick()).catch_unwind() => tick,
                _ = stopping.cancelled() => break,
            };
            if tick.is_err() {
                error!("Quality upgrade tick failed");
            }
        }
        Ok(())
    }

    async fn walk(api: &NavidromePlaylistApi) -> (Vec<LibrarySongRow>, bool) {
        let mut all = Vec::new();
        for page in 0..Self::MAX_PAGES {
            let Some((rows, count)) = api.list_songs(page * Self::PAGE_SIZE, Self::PAGE_SIZE).await else {
                return (all, false);
            };
            all.extend(rows);
            if count < Self::PAGE_SIZE as usize {
                return (all, true);
            }
        }
        (all, false)
    }
}

/// `StringComparer.Ordinal`: UTF-16 code unit order.
pub(crate) fn compare_ordinal(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// The rows of one page of Navidrome's native song list, with how many it held, or `None` when
/// it is not a list. `Err` where the C# threw (a size or bit rate that is not a number).
pub fn parse_songs(root: &Value) -> Result<Option<(Vec<LibrarySongRow>, usize)>, String> {
    let Value::Array(songs) = root else {
        return Ok(None);
    };
    let mut rows = Vec::new();
    let mut count = 0;
    for song in songs {
        count += 1;
        if song.get("missing") == Some(&Value::Bool(true)) {
            continue;
        }
        let (Some(id), Some(path)) = (text(song, "id"), text(song, "path")) else {
            continue;
        };
        if id.is_empty() || path.is_empty() {
            continue;
        }
        rows.push(LibrarySongRow {
            id,
            path,
            library_path: text(song, "libraryPath"),
            size: number(song, "size")?.and_then(|n| n.as_i64()).unwrap_or(0),
            suffix: text(song, "suffix").unwrap_or_default(),
            bit_rate: number(song, "bitRate")?
                .and_then(|n| n.as_i64())
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0),
            title: text(song, "title").unwrap_or_default(),
            artist: text(song, "artist").unwrap_or_default(),
            // A float in Navidrome's native API, unlike Subsonic's whole seconds.
            duration: match song.get("duration") {
                Some(Value::Number(n)) => n.as_f64().map(|d| dotnet::round(d, 0) as i32),
                _ => None,
            },
            album: text(song, "album"),
        });
    }
    Ok(Some((rows, count)))
}

fn text(element: &Value, name: &str) -> Option<String> {
    match element.get(name)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// `TryGetInt64`/`TryGetInt32` on a member: absent is fine, a number is read, and any other
/// kind threw `InvalidOperationException`.
fn number<'a>(element: &'a Value, name: &str) -> Result<Option<&'a serde_json::Number>, String> {
    match element.get(name) {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n)),
        Some(_) => Err(format!(
            "The requested operation requires an element of type 'Number' ({name})."
        )),
    }
}

#[cfg(test)]
#[path = "quality_upgrade_tests.rs"]
mod tests;
