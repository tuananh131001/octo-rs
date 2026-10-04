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
use std::path::PathBuf;
use std::sync::Arc;

use octo_core::settings::{AppSettings, RestartTracker, SettingsFileWriter, SettingsStore};
use tokio_util::sync::CancellationToken;

use crate::services::http_client_factory;
use crate::services::library::{NavidromePlaylistApi, NavidromeSongPathResolver};
use crate::services::local::{ILocalLibraryService, LocalLibraryService};
use crate::services::subsonic::{
    CredentialCheck, NavidromeIdentityService, RequestIdentity, SearchSongOrderCache,
    SubsonicDiscoveryService, SubsonicProxyService,
};
use crate::services::validation::{
    IStartupValidator, StartupValidationOrchestrator, SubsonicStartupValidator,
};
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
    /// `IHostApplicationLifetime`: cancel it (see [`AppInner::stop_application`]) to shut the
    /// process down gracefully, as `StopApplication()` did.
    pub lifetime: CancellationToken,
    /// `AddHttpClient()`: the default client `IHttpClientFactory.CreateClient()` handed out.
    pub http: reqwest::Client,
    /// `AddScoped<SubsonicProxyService>()`. This one has no request (a background scope);
    /// handlers make their request's own with `with_request`.
    pub subsonic_proxy: SubsonicProxyService,
    /// `AddSingleton<NavidromeIdentityService>()` (a cheap handle).
    pub navidrome_identity: NavidromeIdentityService,
    /// `AddSingleton<SubsonicDiscoveryService>()`.
    pub subsonic_discovery: Arc<SubsonicDiscoveryService>,
    /// `AddSingleton<CredentialCheck>()`.
    pub credential_check: Arc<CredentialCheck>,
    /// `AddSingleton<RequestIdentity>()`.
    pub request_identity: Arc<RequestIdentity>,
    /// `AddSingleton<SearchSongOrderCache>()`.
    pub search_song_order_cache: Arc<SearchSongOrderCache>,
    /// `AddSingleton<ILocalLibraryService, LocalLibraryService>()`.
    /// STUB(4-D): the stub service until 4-D lands.
    pub local_library: Arc<dyn ILocalLibraryService>,
    /// `AddSingleton<NavidromeSongPathResolver>()`.
    pub navidrome_song_path_resolver: Arc<NavidromeSongPathResolver>,
    /// `AddSingleton<NavidromePlaylistApi>()`.
    pub navidrome_playlist_api: Arc<NavidromePlaylistApi>,
    /// `AddHostedService<StartupValidationOrchestrator>()` over the `IStartupValidator`s
    /// (`SubsonicStartupValidator`; `SoulseekStartupValidator` joins with 4-A). Run by
    /// [`crate::host::run`] before the listener binds, as the host started it.
    pub startup_validation: Arc<StartupValidationOrchestrator>,
}

/// The Navidrome-facing services, built in dependency order (3-E).
struct NavidromeServices {
    http: reqwest::Client,
    subsonic_proxy: SubsonicProxyService,
    navidrome_identity: NavidromeIdentityService,
    local_library: Arc<dyn ILocalLibraryService>,
    navidrome_song_path_resolver: Arc<NavidromeSongPathResolver>,
    navidrome_playlist_api: Arc<NavidromePlaylistApi>,
    startup_validation: Arc<StartupValidationOrchestrator>,
}

impl NavidromeServices {
    fn build(settings: &Arc<SettingsStore>) -> Self {
        let http = http_client_factory::default_client();
        let navidrome_identity = NavidromeIdentityService::new(Arc::clone(settings), http.clone());
        let local_library: Arc<dyn ILocalLibraryService> = Arc::new(LocalLibraryService::new());
        let validators: Vec<Arc<dyn IStartupValidator>> = vec![Arc::new(SubsonicStartupValidator::new(
            // IOptions<SubsonicSettings>: deliberately the URL Octo started with.
            settings.current().subsonic.url.clone(),
            http.clone(),
        ))];
        NavidromeServices {
            subsonic_proxy: SubsonicProxyService::new(Arc::clone(settings)),
            navidrome_song_path_resolver: Arc::new(NavidromeSongPathResolver::new(
                navidrome_identity.clone(),
                Arc::clone(&local_library),
                http.clone(),
                Arc::clone(settings),
            )),
            navidrome_playlist_api: Arc::new(NavidromePlaylistApi::new(
                http.clone(),
                navidrome_identity.clone(),
                Arc::clone(settings),
            )),
            startup_validation: Arc::new(StartupValidationOrchestrator::new(validators)),
            navidrome_identity,
            local_library,
            http,
        }
    }
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
        let settings = Arc::new(settings);
        let navidrome = NavidromeServices::build(&settings);
        let inner = AppInner {
            settings_writer: Arc::new(SettingsFileWriter::new(settings_path)),
            restart_tracker: Arc::new(restart_tracker),
            settings,
            workers: Arc::new(WorkerSupervisor::new()),
            lifetime: CancellationToken::new(),
            http: navidrome.http,
            subsonic_proxy: navidrome.subsonic_proxy,
            navidrome_identity: navidrome.navidrome_identity,
            subsonic_discovery: Arc::new(SubsonicDiscoveryService::new()),
            credential_check: Arc::new(CredentialCheck::new()),
            request_identity: Arc::new(RequestIdentity::new()),
            search_song_order_cache: Arc::new(SearchSongOrderCache::new()),
            local_library: navidrome.local_library,
            navidrome_song_path_resolver: navidrome.navidrome_song_path_resolver,
            navidrome_playlist_api: navidrome.navidrome_playlist_api,
            startup_validation: navidrome.startup_validation,
        };
        AppState {
            inner: Arc::new(inner),
        }
    }

    /// A state for handler tests: `settings` as the live snapshot, no configuration behind it,
    /// and a settings writer pointed at a fresh path in the temp directory that nothing creates
    /// until a test writes.
    pub fn for_tests(settings: AppSettings) -> AppState {
        let store = SettingsStore::for_tests(settings);
        let path = std::env::temp_dir()
            .join(format!("octo-test-{}", uuid::Uuid::new_v4().simple()))
            .join("settings.json");
        let restart_tracker = Arc::new(RestartTracker::new(&store));
        let settings = Arc::new(store);
        let navidrome = NavidromeServices::build(&settings);
        let inner = AppInner {
            restart_tracker,
            settings,
            settings_writer: Arc::new(SettingsFileWriter::new(path)),
            workers: Arc::new(WorkerSupervisor::new()),
            lifetime: CancellationToken::new(),
            http: navidrome.http,
            subsonic_proxy: navidrome.subsonic_proxy,
            navidrome_identity: navidrome.navidrome_identity,
            subsonic_discovery: Arc::new(SubsonicDiscoveryService::new()),
            credential_check: Arc::new(CredentialCheck::new()),
            request_identity: Arc::new(RequestIdentity::new()),
            search_song_order_cache: Arc::new(SearchSongOrderCache::new()),
            local_library: navidrome.local_library,
            navidrome_song_path_resolver: navidrome.navidrome_song_path_resolver,
            navidrome_playlist_api: navidrome.navidrome_playlist_api,
            startup_validation: navidrome.startup_validation,
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
