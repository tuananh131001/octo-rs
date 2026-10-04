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

use crate::services::admin::{BrowseSessionStore, DirectoryBrowser};
use crate::services::common::{DownloadConcurrency, SoulseekHoldStore};
use crate::services::cover_art::{
    CoverArtAggregator, CoverArtArchiveLookup, DeezerCoverArtLookup, ICoverArtSource, ITunesCoverArtLookup,
    LastFmCoverArtLookup,
};
use crate::services::fingerprint::{
    AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter, MusicBrainzClient,
};
use crate::services::http_client_factory;
use crate::services::last_fm::{LastFmScrobbleService, LastFmService};
use crate::services::library::{NavidromePlaylistApi, NavidromeSongPathResolver};
use crate::services::listen_brainz::ListenBrainzService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::{ILocalLibraryService, LocalLibraryService};
use crate::services::lyrics::LyricsChoiceStore;
use crate::services::metadata::{DeezerMetadataService, DeezerRateLimitHandler, DeezerRateLimiter};
use crate::services::metadata::{GenreBackfillJournal, GenreBackfillStore};
use crate::services::notifications::{DiscordSink, INotificationSink, NotificationService, NtfySink};
use crate::services::soulseek::{ExternalIdRegistry, RadioQueueStore, RejectedPeerRegistry};
use crate::services::subsonic::{
    CredentialCheck, NavidromeIdentityService, RecentScrobbles, RequestIdentity, SearchSongOrderCache,
    SubsonicDiscoveryService, SubsonicProxyService,
};
use crate::services::updates::ReleaseCheck;
use crate::services::updates::UpdateHost;
use crate::services::validation::{
    IStartupValidator, StartupValidationOrchestrator, SubsonicStartupValidator,
};
use crate::services::you_tube::YouTubeResolver;
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

    /// `DeezerRateLimiter` (singleton): the budget every Deezer call spends.
    pub deezer_rate_limiter: Arc<DeezerRateLimiter>,
    /// The named "deezer" HttpClient with `DeezerRateLimitHandler` in its chain. Every Deezer
    /// caller goes through it.
    pub deezer_client: Arc<DeezerRateLimitHandler>,
    /// `DeezerMetadataService` (singleton).
    pub deezer_metadata: Arc<DeezerMetadataService>,
    /// `MusicBrainzClient` (singleton) with its named client.
    pub music_brainz: Arc<MusicBrainzClient>,
    /// `AcoustIdRateLimiter` (singleton).
    pub acoust_id_rate_limiter: Arc<AcoustIdRateLimiter>,
    /// `AcoustIdClient` (singleton), over the named "acoustid" client and its handler.
    pub acoust_id: Arc<AcoustIdClient>,
    /// `ITunesCoverArtLookup`, registered as itself too: downloads and the cover upgrade ask it
    /// for an album's master. Its matches persist to itunes-masters.json beside settings.json.
    pub itunes_cover_art: Arc<ITunesCoverArtLookup>,
    /// `CoverArtAggregator` over the `ICoverArtSource`s in registration order: Deezer, iTunes,
    /// Last.fm.
    pub cover_art_aggregator: Arc<CoverArtAggregator>,
    /// `CoverArtArchiveLookup` (singleton) with its named client.
    pub cover_art_archive: Arc<CoverArtArchiveLookup>,
    /// `DownloadHistoryService`: the fetched-songs log, `downloads-history.json` beside settings.json.
    pub download_history: Arc<DownloadHistoryService>,
    /// `DownloadConcurrency`: how many downloads transfer at once.
    pub download_concurrency: Arc<DownloadConcurrency>,
    /// `SoulseekHoldStore`: hearts waiting out a Soulseek outage, `soulseek-holds.json`.
    pub soulseek_holds: Arc<SoulseekHoldStore>,
    /// `YouTubeResolver`: the yt-dlp shim client (its address captured at startup).
    pub you_tube_resolver: Arc<YouTubeResolver>,
    /// `ExternalIdRegistry`: short external ids, `external-ids.json` (flushed by a worker).
    pub external_id_registry: Arc<ExternalIdRegistry>,
    /// `RadioQueueStore`: recent search and radio queues for prewarming, in memory.
    pub radio_queues: Arc<RadioQueueStore>,
    /// `DirectoryBrowser`: the dashboard's folder picker.
    pub directory_browser: Arc<DirectoryBrowser>,
    /// `BrowseSessionStore`: dashboard sign-ins, `browse-sessions.json`.
    pub browse_sessions: Arc<BrowseSessionStore>,
    /// `RejectedPeerRegistry`: peer files that proved wrong, `rejected-peers.json` (flushed by
    /// a worker), with `Soulseek:RejectedPeerTtlDays` read live.
    pub rejected_peers: Arc<RejectedPeerRegistry>,
    /// `ReleaseCheck`: whether a newer release is out, `update/release.json`; also a worker.
    pub release_check: Arc<ReleaseCheck>,
    /// `LastFmService` (`AddHttpClient<LastFmService>` + singleton): the web service the radio
    /// and the search bar read, its Accept-Language captured at startup.
    pub last_fm: Arc<LastFmService>,
    /// `LastFmScrobbleService` (singleton, "lastfm-scrobble" client): the scrobble queue and the
    /// dashboard's Connect flow, saving sessions through `settings_writer`.
    pub last_fm_scrobbles: Arc<LastFmScrobbleService>,
    /// `ListenBrainzService` (singleton, "listenbrainz" client).
    pub listen_brainz: Arc<ListenBrainzService>,
    /// `NotificationService` over the `INotificationSink`s in registration order (ntfy, then
    /// Discord), sharing the "notifications" client, with Deezer for missing covers.
    pub notifications: Arc<NotificationService>,
    /// `RecentScrobbles`: completed plays reported lately, so one sent twice is learned from once.
    pub recent_scrobbles: Arc<RecentScrobbles>,
}

/// The Last.fm, ListenBrainz and notification services (3-B). None runs in the background
/// until it is used: the scrobble queue drains on a task of its own while plays wait.
struct Integrations {
    last_fm: Arc<LastFmService>,
    last_fm_scrobbles: Arc<LastFmScrobbleService>,
    listen_brainz: Arc<ListenBrainzService>,
    notifications: Arc<NotificationService>,
    recent_scrobbles: Arc<RecentScrobbles>,
}

impl Integrations {
    fn build(
        settings: &Arc<SettingsStore>,
        settings_writer: &Arc<SettingsFileWriter>,
        deezer: &Arc<DeezerMetadataService>,
    ) -> Self {
        let notifications_client = NotificationService::client();
        // IEnumerable<INotificationSink>, so adding a transport is one line here.
        let sinks: Vec<Arc<dyn INotificationSink>> = vec![
            Arc::new(NtfySink::new(notifications_client.clone(), Arc::clone(settings))),
            Arc::new(DiscordSink::new(notifications_client, Arc::clone(settings))),
        ];
        Self {
            last_fm: Arc::new(LastFmService::new(Arc::clone(settings))),
            last_fm_scrobbles: Arc::new(LastFmScrobbleService::new(
                Arc::clone(settings),
                Arc::clone(settings_writer),
            )),
            listen_brainz: Arc::new(ListenBrainzService::new(Arc::clone(settings))),
            notifications: Arc::new(NotificationService::new(
                sinks,
                Arc::clone(settings),
                Some(Arc::clone(deezer)),
            )),
            recent_scrobbles: Arc::new(RecentScrobbles::new()),
        }
    }
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

/// The stores and clients of task 3-F, over the config directory every state file sits in
/// (`Path.GetDirectoryName(SettingsFilePath)`).
struct Stores {
    download_history: Arc<DownloadHistoryService>,
    download_concurrency: Arc<DownloadConcurrency>,
    soulseek_holds: Arc<SoulseekHoldStore>,
    you_tube_resolver: Arc<YouTubeResolver>,
    external_id_registry: Arc<ExternalIdRegistry>,
    radio_queues: Arc<RadioQueueStore>,
    directory_browser: Arc<DirectoryBrowser>,
    browse_sessions: Arc<BrowseSessionStore>,
    rejected_peers: Arc<RejectedPeerRegistry>,
    release_check: Arc<ReleaseCheck>,
}

impl Stores {
    fn build(settings: &Arc<SettingsStore>, config_dir: &Path) -> Stores {
        let ttl_settings = settings.clone();
        Stores {
            download_history: Arc::new(DownloadHistoryService::new(
                config_dir.join("downloads-history.json"),
            )),
            download_concurrency: Arc::new(DownloadConcurrency::new(settings.clone())),
            soulseek_holds: Arc::new(SoulseekHoldStore::new(Some(
                config_dir.join("soulseek-holds.json"),
            ))),
            you_tube_resolver: Arc::new(YouTubeResolver::new(settings)),
            external_id_registry: Arc::new(ExternalIdRegistry::new(Some(
                config_dir.join("external-ids.json"),
            ))),
            radio_queues: Arc::new(RadioQueueStore::new()),
            directory_browser: Arc::new(DirectoryBrowser::new()),
            browse_sessions: Arc::new(BrowseSessionStore::new(Some(
                config_dir.join("browse-sessions.json"),
            ))),
            rejected_peers: Arc::new(RejectedPeerRegistry::new(
                Some(config_dir.join("rejected-peers.json")),
                // Read at every use, so changing the TTL takes effect without a restart.
                Some(Arc::new(move || {
                    ttl_settings.current().soulseek.effective_rejected_peer_ttl_days()
                })),
            )),
            release_check: Arc::new(ReleaseCheck::new(
                config_dir.join("update").join("release.json"),
                settings.clone(),
            )),
        }
    }

    /// The C# flushed the registries from a Timer and once more from Dispose on shutdown, and
    /// ran ReleaseCheck as a hosted service.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let registry = self.external_id_registry.clone();
        workers.register("ExternalIdRegistry", move |token| {
            registry.clone().run_flusher(token)
        });
        let rejected = self.rejected_peers.clone();
        workers.register("RejectedPeerRegistry", move |token| {
            rejected.clone().run_flusher(token)
        });
        let release_check = self.release_check.clone();
        workers.register("ReleaseCheck", move |token| release_check.clone().run(token));
    }
}

/// The metadata clients of `Program.cs`, built in dependency order.
struct MetadataClients {
    deezer_rate_limiter: Arc<DeezerRateLimiter>,
    deezer_client: Arc<DeezerRateLimitHandler>,
    deezer_metadata: Arc<DeezerMetadataService>,
    music_brainz: Arc<MusicBrainzClient>,
    acoust_id_rate_limiter: Arc<AcoustIdRateLimiter>,
    acoust_id: Arc<AcoustIdClient>,
    itunes_cover_art: Arc<ITunesCoverArtLookup>,
    cover_art_aggregator: Arc<CoverArtAggregator>,
    cover_art_archive: Arc<CoverArtArchiveLookup>,
}

impl MetadataClients {
    /// `itunes_cache`: where the iTunes matches persist, or None to keep them in memory only.
    fn build(settings: &Arc<SettingsStore>, itunes_cache: Option<PathBuf>) -> Self {
        let deezer_rate_limiter = Arc::new(DeezerRateLimiter::new());
        let deezer_client = Arc::new(DeezerRateLimitHandler::new(Arc::clone(&deezer_rate_limiter)));
        let deezer_metadata = Arc::new(DeezerMetadataService::new(
            Arc::clone(&deezer_client),
            Arc::clone(settings),
        ));
        let acoust_id_rate_limiter = Arc::new(AcoustIdRateLimiter::new());
        let acoust_id = Arc::new(AcoustIdClient::new(Arc::new(AcoustIdRateLimitHandler::new(
            Arc::clone(&acoust_id_rate_limiter),
        ))));

        // The cover sources take their settings as IOptions: captured here, at construction.
        let current = settings.current();
        let itunes_cover_art = Arc::new(ITunesCoverArtLookup::new(itunes_cache));
        // Cover-art sources, registered in fallback order. The aggregator queries them
        // sequentially; adding or removing a source is a one-line change here.
        let sources: Vec<Arc<dyn ICoverArtSource>> = vec![
            Arc::new(DeezerCoverArtLookup::new(
                Arc::clone(&deezer_client),
                &current.metadata,
            )),
            Arc::clone(&itunes_cover_art) as Arc<dyn ICoverArtSource>,
            Arc::new(LastFmCoverArtLookup::new(&current.last_fm, &current.metadata)),
        ];

        Self {
            deezer_rate_limiter,
            deezer_client,
            deezer_metadata,
            music_brainz: Arc::new(MusicBrainzClient::new()),
            acoust_id_rate_limiter,
            acoust_id,
            itunes_cover_art,
            cover_art_aggregator: Arc::new(CoverArtAggregator::new(sources)),
            cover_art_archive: Arc::new(CoverArtArchiveLookup::new()),
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
        let config_dir = settings_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let settings = Arc::new(settings);
        let stores = Stores::build(&settings, &config_dir);
        // Matches survive a restart, so a library is not sent back to Apple album by album.
        let clients = MetadataClients::build(&settings, Some(config_dir.join("itunes-masters.json")));
        let workers = Arc::new(WorkerSupervisor::new());
        stores.register_workers(&workers);

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

        let navidrome = NavidromeServices::build(&settings);
        // The C# lookup's 15-second Timer, and its Dispose's last write.
        let itunes = Arc::clone(&clients.itunes_cover_art);
        workers.register("ITunesCoverArtLookup", move |stopping| {
            let itunes = Arc::clone(&itunes);
            async move { itunes.run_flush_loop(stopping).await }
        });
        let settings_writer = Arc::new(SettingsFileWriter::new(settings_path));
        let integrations = Integrations::build(&settings, &settings_writer, &clients.deezer_metadata);
        let inner = AppInner {
            settings_writer,
            restart_tracker: Arc::new(restart_tracker),
            settings,
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
            download_history: stores.download_history,
            download_concurrency: stores.download_concurrency,
            soulseek_holds: stores.soulseek_holds,
            you_tube_resolver: stores.you_tube_resolver,
            external_id_registry: stores.external_id_registry,
            radio_queues: stores.radio_queues,
            directory_browser: stores.directory_browser,
            browse_sessions: stores.browse_sessions,
            rejected_peers: stores.rejected_peers,
            release_check: stores.release_check,
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
            deezer_rate_limiter: clients.deezer_rate_limiter,
            deezer_client: clients.deezer_client,
            deezer_metadata: clients.deezer_metadata,
            music_brainz: clients.music_brainz,
            acoust_id_rate_limiter: clients.acoust_id_rate_limiter,
            acoust_id: clients.acoust_id,
            itunes_cover_art: clients.itunes_cover_art,
            cover_art_aggregator: clients.cover_art_aggregator,
            cover_art_archive: clients.cover_art_archive,
            last_fm: integrations.last_fm,
            last_fm_scrobbles: integrations.last_fm_scrobbles,
            listen_brainz: integrations.listen_brainz,
            notifications: integrations.notifications,
            recent_scrobbles: integrations.recent_scrobbles,
        };
        AppState {
            inner: Arc::new(inner),
        }
    }

    /// A state for handler tests: `settings` as the live snapshot, no configuration behind it,
    /// and a settings writer, the update folder and the state files pointed at a fresh directory in the temp
    /// directory that nothing creates until a test writes. No workers are registered.
    pub fn for_tests(settings: AppSettings) -> AppState {
        let store = SettingsStore::for_tests(settings);
        let config_dir = std::env::temp_dir().join(format!("octo-test-{}", uuid::Uuid::new_v4().simple()));
        let restart_tracker = Arc::new(RestartTracker::new(&store));
        let settings = Arc::new(store);
        let stores = Stores::build(&settings, &config_dir);
        let navidrome = NavidromeServices::build(&settings);
        // The clients point at the real hosts but ask nothing until a test calls them, and
        // the iTunes matches stay in memory.
        let clients = MetadataClients::build(&settings, None);
        // The Last.fm, ListenBrainz and notification clients point at the real hosts too, and
        // the default settings give none of them a key, token or URL to use.
        let settings_writer = Arc::new(SettingsFileWriter::new(config_dir.join("settings.json")));
        let integrations = Integrations::build(&settings, &settings_writer, &clients.deezer_metadata);
        let inner = AppInner {
            restart_tracker,
            settings,
            settings_writer,
            workers: Arc::new(WorkerSupervisor::new()),
            lyrics_choice_store: Arc::new(LyricsChoiceStore::new(None)),
            genre_backfill_store: Arc::new(GenreBackfillStore::new(None)),
            genre_backfill_journal: Arc::new(GenreBackfillJournal::new(None)),
            update_host: Arc::new(UpdateHost::new(config_dir.join("update"), Clock::system())),
            lifetime: CancellationToken::new(),
            download_history: stores.download_history,
            download_concurrency: stores.download_concurrency,
            soulseek_holds: stores.soulseek_holds,
            you_tube_resolver: stores.you_tube_resolver,
            external_id_registry: stores.external_id_registry,
            radio_queues: stores.radio_queues,
            directory_browser: stores.directory_browser,
            browse_sessions: stores.browse_sessions,
            rejected_peers: stores.rejected_peers,
            release_check: stores.release_check,
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
            deezer_rate_limiter: clients.deezer_rate_limiter,
            deezer_client: clients.deezer_client,
            deezer_metadata: clients.deezer_metadata,
            music_brainz: clients.music_brainz,
            acoust_id_rate_limiter: clients.acoust_id_rate_limiter,
            acoust_id: clients.acoust_id,
            itunes_cover_art: clients.itunes_cover_art,
            cover_art_aggregator: clients.cover_art_aggregator,
            cover_art_archive: clients.cover_art_archive,
            last_fm: integrations.last_fm,
            last_fm_scrobbles: integrations.last_fm_scrobbles,
            listen_brainz: integrations.listen_brainz,
            notifications: integrations.notifications,
            recent_scrobbles: integrations.recent_scrobbles,
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

    /// The iTunes matches are flushed by a worker, and the Deezer metadata service and cover
    /// lookup share one client, and so one budget.
    #[test]
    fn the_metadata_clients_are_wired_as_program_cs_registered_them() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        assert!(
            state
                .workers
                .status()
                .iter()
                .any(|w| w.name == "ITunesCoverArtLookup")
        );
        assert!(Arc::ptr_eq(
            state.deezer_client.limiter(),
            &state.deezer_rate_limiter
        ));
    }

    /// The sinks in `Program.cs` registration order, ntfy then Discord, and nothing of 3-B's
    /// sent anywhere with the default settings.
    #[tokio::test]
    async fn the_notification_sinks_and_scrobblers_are_wired_as_program_cs_registered_them() {
        let state = AppState::for_tests(AppSettings::default());
        let results = state.notifications.send_test().await;
        let sinks: Vec<(&str, bool)> = results.iter().map(|r| (r.sink.as_str(), r.configured)).collect();
        assert_eq!(sinks, [("ntfy", false), ("discord", false)]);
        assert!(!state.last_fm.has_api_key());
        assert!(!state.last_fm_scrobbles.is_ready());
        assert!(!state.listen_brainz.is_enabled_for("alice"));
        assert!(
            state
                .recent_scrobbles
                .first_report("alice", "1", None, chrono::Utc::now())
        );
    }

    #[test]
    fn the_store_flushers_and_the_release_check_run_as_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in ["ExternalIdRegistry", "RejectedPeerRegistry", "ReleaseCheck"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        assert!(
            AppState::for_tests(AppSettings::default())
                .workers
                .status()
                .is_empty()
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
