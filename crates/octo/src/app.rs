//! The application's shared state: one `Arc` per service, standing in for the singletons
//! `Program.cs` registered with ASP.NET's container.
//!
//! # Adding a service (the pattern every porter follows)
//!
//! 1. Add a field to [`AppInner`], holding the service as `Arc<YourService>` (or a type that is
//!    already a cheap handle). Name it after the C# class in snake case
//!    (`SubsonicProxyService` → `subsonic_proxy`), and put a `///` line on it naming the C#
//!    registration it replaces.
//! 2. Construct it in [`AppState::build`] (production) **and** in [`AppState::for_tests`]
//!    (handler tests), in dependency order. A service the C# resolved eagerly at startup
//!    (`app.Services.GetRequiredService<T>()` in `Program.cs`) is built here; a lazily built one
//!    is still built here, which is fine unless its constructor does I/O.
//! 3. A C# class registered "Singleton AND hosted" is one `Arc` field plus a worker:
//!    register its loop with `state.workers.register("Name", ...)` in [`AppState::build`],
//!    capturing a clone of the `Arc`. Never start work from a constructor.
//! 4. Settings: services read `state.settings.current()` at the point of use
//!    (`IOptionsMonitor`), or copy a value out in their constructor and say so (`IOptions`).
//!
//! Handlers take `State(state): State<AppState>` and reach services as `state.settings`,
//! `state.restart_tracker`, ... through [`Deref`](std::ops::Deref).

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use octo_core::common::Clock;
use octo_core::settings::{AppSettings, RestartTracker, SettingsFileWriter, SettingsStore};
use tokio_util::sync::CancellationToken;

use crate::services::lyrics::LyricsChoiceStore;
use crate::services::metadata::{GenreBackfillJournal, GenreBackfillStore};
use crate::services::updates::UpdateHost;
use crate::workers::WorkerSupervisor;

/// Cheap to clone; handlers and workers receive it by value.
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<AppInner>,
}

impl Deref for AppState {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.inner
    }
}

/// The services. Filled in as each part of the port lands.
pub struct AppInner {
    /// The live settings (`IOptionsMonitor<T>` for every section, and raw `IConfiguration`).
    pub settings: Arc<SettingsStore>,
    /// `RestartTracker`, snapshotted when the state is built, before anything can change a
    /// setting ("Resolved here so it snapshots the values this process actually started with").
    pub restart_tracker: Arc<RestartTracker>,
    /// `SettingsFileWriter`, writing the store's settings.json.
    pub settings_writer: Arc<SettingsFileWriter>,
    /// The background workers (`AddHostedService`).
    pub workers: Arc<WorkerSupervisor>,
    /// `LyricsChoiceStore`: the lyrics pins, `<config>/lyrics-choices.json`.
    pub lyrics_choice_store: Arc<LyricsChoiceStore>,
    /// `GenreBackfillStore`: the genre backfill run, `<config>/genre-backfill.json`. Its
    /// coalescing flush (a timer in C#) is the "GenreBackfillStore" worker.
    pub genre_backfill_store: Arc<GenreBackfillStore>,
    /// `GenreBackfillJournal`: the genre undo log, `<config>/genre-backfill-journal.jsonl`.
    pub genre_backfill_journal: Arc<GenreBackfillJournal>,
    /// `UpdateHost`: the handshake files with the host updater, `<config>/update/`.
    pub update_host: Arc<UpdateHost>,
    /// `IHostApplicationLifetime`: cancel it (see [`AppInner::stop_application`]) to shut the
    /// process down gracefully, as `StopApplication()` did.
    pub lifetime: CancellationToken,
}

impl AppInner {
    /// `IHostApplicationLifetime.StopApplication()`: begins a graceful shutdown.
    pub fn stop_application(&self) {
        self.lifetime.cancel();
    }
}

impl AppState {
    /// The production state over a loaded settings store. Workers are registered here but not
    /// started; [`crate::host::run`] starts them once the listener is bound.
    pub fn build(settings: SettingsStore) -> AppState {
        let settings_path = settings
            .settings_path()
            .map(PathBuf::from)
            .unwrap_or_else(SettingsStore::default_settings_path);
        let restart_tracker = RestartTracker::new(&settings);
        // Every state file sits beside settings.json (`Path.GetDirectoryName(SettingsFilePath)`).
        let config_dir = settings_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let workers = Arc::new(WorkerSupervisor::new());

        let genre_backfill_store = Arc::new(GenreBackfillStore::new(Some(
            config_dir.join("genre-backfill.json"),
        )));
        let flushed = genre_backfill_store.clone();
        workers.register("GenreBackfillStore", move |stopping| {
            let store = flushed.clone();
            async move {
                store.run_flusher(stopping).await;
                Ok(())
            }
        });

        let inner = AppInner {
            settings_writer: Arc::new(SettingsFileWriter::new(settings_path)),
            restart_tracker: Arc::new(restart_tracker),
            settings: Arc::new(settings),
            workers,
            lyrics_choice_store: Arc::new(LyricsChoiceStore::new(Some(
                config_dir.join("lyrics-choices.json"),
            ))),
            genre_backfill_store,
            genre_backfill_journal: Arc::new(GenreBackfillJournal::new(Some(
                config_dir.join("genre-backfill-journal.jsonl"),
            ))),
            update_host: Arc::new(UpdateHost::new(config_dir.join("update"), Clock::system())),
            lifetime: CancellationToken::new(),
        };
        AppState {
            inner: Arc::new(inner),
        }
    }

    /// A state for handler tests: `settings` as the live snapshot, no configuration behind it,
    /// a settings writer and update folder pointed at a fresh directory in the temp directory
    /// that nothing creates until a test writes, and the other stores in memory.
    pub fn for_tests(settings: AppSettings) -> AppState {
        let store = SettingsStore::for_tests(settings);
        let config_dir = std::env::temp_dir().join(format!("octo-test-{}", uuid::Uuid::new_v4().simple()));
        let inner = AppInner {
            restart_tracker: Arc::new(RestartTracker::new(&store)),
            settings: Arc::new(store),
            settings_writer: Arc::new(SettingsFileWriter::new(config_dir.join("settings.json"))),
            workers: Arc::new(WorkerSupervisor::new()),
            lyrics_choice_store: Arc::new(LyricsChoiceStore::new(None)),
            genre_backfill_store: Arc::new(GenreBackfillStore::new(None)),
            genre_backfill_journal: Arc::new(GenreBackfillJournal::new(None)),
            update_host: Arc::new(UpdateHost::new(config_dir.join("update"), Clock::system())),
            lifetime: CancellationToken::new(),
        };
        AppState {
            inner: Arc::new(inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Program.cs` resolved RestartTracker right after `Build()`, so it holds the values the
    /// process started with; a later change shows as pending.
    #[test]
    fn the_restart_tracker_snapshots_the_settings_the_state_was_built_with() {
        let env = vec![("Subsonic__Url".to_string(), "http://navidrome:4533".to_string())];
        let store = SettingsStore::from_env(env, None);
        let state = AppState::build(store);
        assert!(state.restart_tracker.pending(&*state.settings).is_empty());
        state
            .settings
            .set_raw("Subsonic:Url", Some("http://elsewhere:4533"));
        assert_eq!(
            state.restart_tracker.pending(&*state.settings),
            vec!["Subsonic:Url".to_string()]
        );
        assert_eq!(
            state.settings_writer.file_path(),
            SettingsStore::default_settings_path()
        );
    }

    #[test]
    fn stop_application_cancels_the_lifetime() {
        let state = AppState::for_tests(AppSettings::default());
        assert!(!state.lifetime.is_cancelled());
        state.stop_application();
        assert!(state.lifetime.is_cancelled());
    }
}
