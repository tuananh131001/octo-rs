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
use octo_core::tagging::{CatalogLookup, FingerprintVerifier, ReleaseIdentifier, ReleaseLookup, TagPreview};
use octo_media::audio::{AudioFingerprinter, ILoudnessMeter, LoudnessMeter, SpectrumAnalyzer};
use octo_media::tags::TagWriterExtras;
use octo_subsonic::SubsonicResponseBuilder;
use tokio_util::sync::CancellationToken;

use crate::services::admin::{BrowseSessionStore, DirectoryBrowser};
use crate::services::common::{
    AcquisitionActivity, AcquisitionTracker, AcquisitionWorker, CacheCleanupService, CatalogBlanks,
    DownloadConcurrency, DownloadCore, DownloadServices, ExternalSearchService, HeartAcquisitionCoordinator,
    HeartCoordinatorExtras, MeterPreview, ReviewQueue, SoulseekHoldResumer, SoulseekHoldStore, StarNavidrome,
    StarOnArrival, TrackAcquisitionQueue, TrackerServices,
};
use crate::services::cover_art::{
    CoverArtAggregator, CoverArtArchiveLookup, DeezerCoverArtLookup, DownloadCoverResolver, ICoverArtSource,
    ITunesCoverArtLookup, LastFmCoverArtLookup,
};

use crate::services::fingerprint::{
    AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter, DownloadVerificationService,
    MusicBrainzClient,
};
use crate::services::http_client_factory;
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::{
    FfmpegLastFmRadioAudioTranscoder, LastFmRadioRecommendationService, LastFmRadioRefreshQueue,
    LastFmRadioRefreshWorker, LastFmRadioStateStore, LastFmRadioStreamService, LastFmRadioStreamServiceParts,
    LastFmRadioStreamSessionStore, LastFmRadioTrackCache, LastFmRadioWarmupService, LastFmScrobbleService,
    LastFmService, RandomRadioTuneInSelector,
};
use crate::services::library::GeneratedPlaylistService;
use crate::services::library::{
    HeartOwnership, LibraryActionExecutor, LibraryActionExecutorParts, LibraryActionJournal,
    LibraryActionPlaylistWorker, LibraryActionQuarantine, LibraryActionRatingWorker, LibraryOwnership,
    NavidromePlaylistApi, NavidromeSongPathResolver, NoticeQueue, OwnershipNavidrome, UpgradeQueue,
    UpgradeSources,
};
use crate::services::lidarr::{
    ILidarrHeartAcquisitionService, ILidarrTrackFetcher, LidarrAlbumClaims, LidarrClient,
    LidarrHeartAcquisitionService, LidarrHeartExtras, LidarrImportHandoff, LidarrTrackFetcher,
};
use crate::services::listen_brainz::ListenBrainzService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::{ILocalLibraryService, LocalLibraryService};
use crate::services::lyrics::{
    LyricsChoiceService, LyricsChoiceStore, LyricsLibraryStore, LyricsLibraryWorker, LyricsService,
    LyricsSidecarWriter, LyricsUndoJournal, lyrics_http,
};
use crate::services::metadata::{DeezerMetadataService, DeezerRateLimitHandler, DeezerRateLimiter};
use crate::services::metadata::{GenreBackfillJournal, GenreBackfillStore};
use crate::services::notifications::{DiscordSink, INotificationSink, NotificationService, NtfySink};
use crate::services::soulseek::soulseek_download_service::{SoulseekDownloadParts, SoulseekDownloadService};
use crate::services::soulseek::soulseek_metadata_service::LastFmTrackLengths;
use crate::services::soulseek::{
    ExternalIdRegistry, ISoulseekLink, RadioQueueStore, RejectedPeerRegistry, SoulseekClient, SoulseekLink,
    SoulseekMetadataService, SoulseekStartupValidator,
};
use crate::services::subsonic::new_subsonic_response_builder;
use crate::services::subsonic::{
    CredentialCheck, NavidromeIdentityService, RecentScrobbles, RequestIdentity, SearchSongOrderCache,
    SubsonicDiscoveryService, SubsonicProxyService, SyncCatalogService,
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
    /// `LyricsService`, over the `ILyricsSource`s (KuGou on the `kugou` client; LRCLIB,
    /// NetEase and lyrics.ovh on the `lyrics` client).
    pub lyrics_service: Arc<LyricsService>,
    /// `LyricsChoiceService`: choosing lyrics by hand, over the pins.
    pub lyrics_choice_service: Arc<LyricsChoiceService>,
    /// `LyricsSidecarWriter`, singleton and hosted: its queue is the "LyricsSidecarWriter" worker.
    pub lyrics_sidecar_writer: Arc<LyricsSidecarWriter>,
    /// `LyricsUndoJournal`: what the lyrics page's Save wrote over, `<config>/lyrics-undo.jsonl`.
    pub lyrics_undo_journal: Arc<LyricsUndoJournal>,
    /// `LyricsLibraryStore`: the "find lyrics for the library" run, `<config>/lyrics-library.json`.
    /// Its coalescing flush (a timer in C#) is the "LyricsLibraryStore" worker.
    pub lyrics_library_store: Arc<LyricsLibraryStore>,
    /// `LyricsLibraryWorker`, singleton and hosted: its queue is the "LyricsLibraryWorker" worker.
    pub lyrics_library_worker: Arc<LyricsLibraryWorker>,
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
    /// `AddSingleton<ILocalLibraryService, LocalLibraryService>()`: `.mappings.json` in the
    /// configured download directory, and the scan trigger.
    pub local_library: Arc<dyn ILocalLibraryService>,
    /// `AddSingleton<NavidromeSongPathResolver>()`.
    pub navidrome_song_path_resolver: Arc<NavidromeSongPathResolver>,
    /// `AddSingleton<NavidromePlaylistApi>()`.
    pub navidrome_playlist_api: Arc<NavidromePlaylistApi>,
    /// `AddHostedService<StartupValidationOrchestrator>()` over the `IStartupValidator`s
    /// (`SubsonicStartupValidator`, then `SoulseekStartupValidator`). Run by
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
    /// `SubsonicResponseBuilder`: every Subsonic answer Octo writes itself, over the external id
    /// registry, with `Subsonic:WaitForLosslessOnPlay` captured at startup (`IOptions`).
    /// The app-type answers come with `services::subsonic::SubsonicResponseBuilderExt`.
    pub subsonic_response_builder: Arc<SubsonicResponseBuilder>,
    /// `AddSingleton<SoulseekClient>()`: slskd's REST client (a cheap handle), its address and
    /// login captured at startup.
    pub soulseek_client: SoulseekClient,
    /// `AddSingleton<SoulseekLink>()`, also registered as `ISoulseekLink`: slskd's Soulseek
    /// login, read live, for the dashboard and for downloads that wait out an outage.
    pub soulseek_link: Arc<SoulseekLink>,
    /// `AddSingleton<IMusicMetadataService, SoulseekMetadataService>()`.
    pub music_metadata: Arc<SoulseekMetadataService>,
    /// `AddSingleton<IDownloadService, SoulseekDownloadService>()`: the download base over the
    /// Soulseek backend, which hearts, plays, upgrades, Lidarr and library actions all go through.
    pub download_service: Arc<dyn IDownloadService>,
    /// `AddSingleton<LidarrClient>()`: the Lidarr v1 client, its address and key read live.
    pub lidarr_client: Arc<LidarrClient>,
    /// `LidarrAlbumClaims`: which Lidarr albums a heart or an upgrade is working on.
    pub lidarr_album_claims: Arc<LidarrAlbumClaims>,
    /// `LidarrImportHandoff`: the files a Lidarr heart brought in, waiting for the pipeline.
    pub lidarr_import_handoff: Arc<LidarrImportHandoff>,
    /// `ILidarrTrackFetcher` (`LidarrTrackFetcher`): one song through Lidarr, for a replacement.
    pub lidarr_track_fetcher: Arc<dyn ILidarrTrackFetcher>,
    /// `ILidarrHeartAcquisitionService` (`LidarrHeartAcquisitionService`).
    pub lidarr_hearts: Arc<dyn ILidarrHeartAcquisitionService>,
    /// `TrackAcquisitionQueue`: permanent-copy fetches, drained by the "AcquisitionWorker" worker.
    pub track_acquisition_queue: Arc<TrackAcquisitionQueue>,
    /// `IAcquisitionActivity` (`AcquisitionActivity(sp)`): whether anything is downloading.
    pub acquisition_activity: Arc<AcquisitionActivity>,
    /// `AcquisitionTracker(logger, sp)`: where each hearted download has got to, in memory.
    pub acquisition_tracker: Arc<AcquisitionTracker>,
    /// `StarOnArrival`, built eagerly as `Program.cs` resolved it after `Build()`, so it is
    /// listening when the first download finishes.
    pub star_on_arrival: Arc<StarOnArrival>,
    /// `UpgradeQueue`. STUB(5-B): an in-memory stand-in until 5-B lands.
    pub upgrade_queue: Arc<UpgradeQueue>,
    /// `UpgradeSources`: where Better quality looks, over the Soulseek link.
    pub upgrade_sources: Arc<UpgradeSources>,
    /// `LibraryOwnership`: whether a song is already in the library.
    pub library_ownership: Arc<LibraryOwnership>,
    /// `HeartOwnership`: asked before a heart goes anywhere.
    pub heart_ownership: Arc<HeartOwnership>,
    /// `HeartAcquisitionCoordinator`; `SoulseekHoldResumer` is its "SoulseekHoldResumer" worker.
    pub heart_acquisition_coordinator: Arc<HeartAcquisitionCoordinator>,
    /// `ExternalSearchService`: the discovery half of a search, once per query.
    pub external_search: Arc<ExternalSearchService>,
    /// `LastFmRadioStateStore`: per-listener plays and stations, `lastfm-radio-state.json`.
    pub last_fm_radio_state: Arc<LastFmRadioStateStore>,
    /// `LastFmRadioRefreshQueue` (singleton): the rebuilds waiting for the refresh worker.
    pub last_fm_radio_refresh_queue: Arc<LastFmRadioRefreshQueue>,
    /// `LastFmRadioStreamSessionStore` (singleton): the radio stream tokens, in memory.
    pub last_fm_radio_stream_sessions: Arc<LastFmRadioStreamSessionStore>,
    /// `LastFmRadioTrackCache` (singleton): `<temp>/octo-cache/radio`.
    pub last_fm_radio_track_cache: Arc<LastFmRadioTrackCache>,
    /// `LastFmRadioRecommendationService` (scoped in C#; it holds nothing between builds).
    pub last_fm_radio_recommendations: Arc<LastFmRadioRecommendationService>,
    /// `LastFmRadioStreamService` (scoped) over the background proxy, with the singleton
    /// transcoder (`FfmpegLastFmRadioAudioTranscoder`) and tune-in selector
    /// (`RandomRadioTuneInSelector`). A handler takes its request's scope with `scoped(proxy)`.
    pub last_fm_radio_streams: LastFmRadioStreamService,
    /// `LastFmRadioWarmupService`, singleton AND hosted: its loop is the
    /// "LastFmRadioWarmupService" worker.
    pub last_fm_radio_warmup: Arc<LastFmRadioWarmupService>,
    /// `LastFmRadioRefreshWorker` (hosted): the "LastFmRadioRefreshWorker" worker, and the only
    /// settings `OnChange` subscriber.
    pub last_fm_radio_refresh_worker: Arc<LastFmRadioRefreshWorker>,
    /// `GeneratedPlaylistService`: the genre and decade mixes, `generated-playlists.json`.
    pub generated_playlists: Arc<GeneratedPlaylistService>,
    /// `AddSingleton<SyncCatalogService>()`: the discovery catalog appended to a syncing
    /// client's library walk, built over the background proxy (the C# made a scope per build).
    pub sync_catalog: Arc<SyncCatalogService>,
    /// `LibraryActionJournal`: the write-ahead journal, `<config>/library-actions.json`. Its
    /// 10-second flush (a timer in C#) is the "LibraryActionJournal" worker.
    pub library_action_journal: Arc<LibraryActionJournal>,
    /// `LibraryActionQuarantine` (singleton).
    pub library_action_quarantine: Arc<LibraryActionQuarantine>,
    /// `LibraryActionExecutor` (singleton): the only code in library actions that touches a file.
    pub library_action_executor: Arc<LibraryActionExecutor>,
    /// `LibraryActionRatingWorker`, singleton AND hosted: the controller enqueues into the
    /// "LibraryActionRatingWorker" worker. (`LibraryActionPlaylistWorker` is hosted only, and
    /// `LibraryActionPlaylistProvisioner` is scoped: built per request over its proxy.)
    pub library_action_rating_worker: Arc<LibraryActionRatingWorker>,
    /// `NoticeQueue`. STUB(5-B): in memory, and only what library actions call, until 5-B lands.
    pub notice_queue: Arc<NoticeQueue>,
    /// `ILoudnessMeter` (`LoudnessMeter`): ReplayGain for downloads and the tag preview.
    pub loudness_meter: Arc<dyn ILoudnessMeter>,
    /// `AudioFingerprinter`: fpcalc, for download verification.
    pub audio_fingerprinter: Arc<AudioFingerprinter>,
    /// `SpectrumAnalyzer`: the transcode check.
    pub spectrum_analyzer: Arc<SpectrumAnalyzer>,
    /// `ReleaseIdentifier`: what a downloaded file is, for the download base and the preview.
    pub release_identifier: Arc<ReleaseIdentifier>,
    /// `TagPreview`: "Try it on a song".
    pub tag_preview: Arc<TagPreview>,
    /// `DownloadVerificationService`: the fingerprint and ISRC checks of a finished download.
    pub download_verification: Arc<DownloadVerificationService>,
    /// `DownloadCoverResolver`: the download-time cover chain (#51).
    pub download_cover_resolver: Arc<DownloadCoverResolver>,
}

/// The tagging and verification services of task 4-B, which the download base
/// (`services::common::BaseDownloadService`) and the tag preview are built over.
struct Tagging {
    loudness_meter: Arc<dyn ILoudnessMeter>,
    audio_fingerprinter: Arc<AudioFingerprinter>,
    spectrum_analyzer: Arc<SpectrumAnalyzer>,
    release_identifier: Arc<ReleaseIdentifier>,
    tag_preview: Arc<TagPreview>,
    download_verification: Arc<DownloadVerificationService>,
    download_cover_resolver: Arc<DownloadCoverResolver>,
}

impl Tagging {
    fn build(settings: &Arc<SettingsStore>, clients: &MetadataClients) -> Tagging {
        let loudness_meter: Arc<dyn ILoudnessMeter> = Arc::new(LoudnessMeter::new());
        let audio_fingerprinter = Arc::new(AudioFingerprinter::new());
        let spectrum_analyzer = Arc::new(SpectrumAnalyzer::new());
        let release_identifier = Arc::new(ReleaseIdentifier::new(
            Arc::clone(settings),
            Arc::new(TagWriterExtras),
            Some(Arc::clone(&clients.music_brainz) as Arc<dyn ReleaseLookup>),
            Some(Arc::clone(&clients.deezer_metadata) as Arc<dyn CatalogLookup>),
        ));
        let download_verification = Arc::new(DownloadVerificationService::new(
            Arc::clone(&audio_fingerprinter),
            Arc::clone(&clients.acoust_id),
            Arc::clone(settings),
            Some(Arc::clone(&clients.music_brainz)),
            Some(Arc::clone(&spectrum_analyzer)),
        ));
        let tag_preview = Arc::new(TagPreview::new(
            Arc::clone(settings),
            Arc::clone(&release_identifier),
            Arc::new(TagWriterExtras),
            Some(Arc::clone(&download_verification) as Arc<dyn FingerprintVerifier>),
            Some(Arc::new(MeterPreview(Arc::clone(&loudness_meter)))),
            Arc::new(CatalogBlanks),
        ));
        let download_cover_resolver = Arc::new(DownloadCoverResolver::new(
            Arc::clone(&clients.cover_art_archive),
            Arc::clone(&clients.cover_art_aggregator),
            http_client_factory::default_client(),
            Arc::clone(settings),
            Some(Arc::clone(&clients.itunes_cover_art)),
        ));
        Tagging {
            loudness_meter,
            audio_fingerprinter,
            spectrum_analyzer,
            release_identifier,
            tag_preview,
            download_verification,
            download_cover_resolver,
        }
    }
}

/// What the download service (4-C) is built over beyond the acquisition pipeline's own
/// services: the tagging and verification services, the lyrics writer, slskd's client and the
/// notice queue it asks people through.
struct DownloadDeps<'a> {
    tagging: &'a Tagging,
    lyrics: &'a Arc<LyricsSidecarWriter>,
    soulseek_client: &'a SoulseekClient,
    notice_queue: &'a Arc<NoticeQueue>,
}

/// `NoticeQueue.AddReview` as the download base asks for it.
struct NoticeReviews(Arc<NoticeQueue>);

impl ReviewQueue for NoticeReviews {
    fn add_review(
        &self,
        owner: &str,
        local_path: &str,
        song: &octo_core::models::domain::Song,
        verdict: &octo_core::fingerprint::VerificationResult,
    ) -> bool {
        self.0.add_review(owner, local_path, song, verdict)
    }
}

/// The acquisition pipeline of task 4-D, over the services it routes to. The C# broke two
/// cycles with lazy `IServiceProvider` lookups: `AcquisitionActivity` holds the download
/// service weakly, set once it exists, and `StarOnArrival` listens to the tracker through a weak
/// reference. The tracker's own lazy lookups (the song path resolver, the identity, the library
/// service) depend on nothing of this, so they are handed over at construction.
struct Acquisition {
    download_service: Arc<dyn IDownloadService>,
    lidarr_client: Arc<LidarrClient>,
    lidarr_album_claims: Arc<LidarrAlbumClaims>,
    lidarr_import_handoff: Arc<LidarrImportHandoff>,
    lidarr_track_fetcher: Arc<dyn ILidarrTrackFetcher>,
    lidarr_hearts: Arc<dyn ILidarrHeartAcquisitionService>,
    track_acquisition_queue: Arc<TrackAcquisitionQueue>,
    acquisition_activity: Arc<AcquisitionActivity>,
    acquisition_tracker: Arc<AcquisitionTracker>,
    star_on_arrival: Arc<StarOnArrival>,
    upgrade_queue: Arc<UpgradeQueue>,
    upgrade_sources: Arc<UpgradeSources>,
    library_ownership: Arc<LibraryOwnership>,
    heart_ownership: Arc<HeartOwnership>,
    heart_acquisition_coordinator: Arc<HeartAcquisitionCoordinator>,
    external_search: Arc<ExternalSearchService>,
}

impl Acquisition {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        integrations: &Integrations,
        clients: &MetadataClients,
        music_metadata: Arc<dyn IMusicMetadataService>,
        soulseek_link: Arc<dyn ISoulseekLink>,
        downloads: DownloadDeps<'_>,
    ) -> Acquisition {
        let track_acquisition_queue = Arc::new(TrackAcquisitionQueue::new());
        let acquisition_activity = Arc::new(AcquisitionActivity::new(track_acquisition_queue.clone()));
        let acquisition_tracker = Arc::new(AcquisitionTracker::new(
            Some(TrackerServices {
                identity: navidrome.navidrome_identity.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
                library: navidrome.local_library.clone(),
            }),
            Clock::system(),
        ));
        let star_on_arrival = StarOnArrival::new(
            acquisition_tracker.clone(),
            Some(StarNavidrome {
                proxy: navidrome.subsonic_proxy.clone(),
                identity: navidrome.navidrome_identity.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
            }),
            settings.clone(),
            Clock::system(),
        );
        let upgrade_queue = Arc::new(UpgradeQueue::new());
        let upgrade_sources = Arc::new(UpgradeSources::new(settings.clone(), Some(soulseek_link.clone())));
        let library_ownership = Arc::new(LibraryOwnership::new(
            settings.clone(),
            Some(OwnershipNavidrome {
                identity: navidrome.navidrome_identity.clone(),
                http: navidrome.http.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
            }),
            navidrome.local_library.clone(),
        ));

        // Lidarr (4-E). The heart service's lazy IServiceProvider lookups (the download
        // pipeline, the journal, the ownership check, the upgrade queue) are all built by now.
        let lidarr_client = Arc::new(LidarrClient::new(settings.clone()));
        let lidarr_album_claims = Arc::new(LidarrAlbumClaims::new());
        let lidarr_import_handoff = Arc::new(LidarrImportHandoff::new());
        let lidarr_track_fetcher: Arc<dyn ILidarrTrackFetcher> = Arc::new(LidarrTrackFetcher::new(
            lidarr_client.clone(),
            settings.clone(),
            navidrome.navidrome_identity.clone(),
            lidarr_album_claims.clone(),
            Some(clients.music_brainz.clone()),
        ));

        // The download service (4-C): the base's service-provider lookups are the services built
        // above, and the Soulseek backend's optional ones the link and Lidarr's two.
        let (download_base, _) = SoulseekDownloadService::build(
            DownloadCore {
                settings: settings.clone(),
                local_library: navidrome.local_library.clone(),
                metadata: music_metadata.clone(),
                navidrome_identity: navidrome.navidrome_identity.clone(),
                history: stores.download_history.clone(),
                notifications: integrations.notifications.clone(),
            },
            DownloadServices {
                concurrency: Some(stores.download_concurrency.clone()),
                tracker: Some(acquisition_tracker.clone()),
                ownership: Some(library_ownership.clone()),
                upgrade_queue: Some(upgrade_queue.clone()),
                upgrade_sources: Some(upgrade_sources.clone()),
                action_journal: Some(stores.library_action_journal.clone()),
                review_queue: Some(Arc::new(NoticeReviews(downloads.notice_queue.clone()))),
                // PlaylistSyncService is never registered (see known-diffs).
                playlist_sync: None,
                deezer: Some(clients.deezer_metadata.clone()),
                music_brainz: Some(clients.music_brainz.clone()),
                release_identifier: Some(downloads.tagging.release_identifier.clone()),
                loudness_meter: Some(downloads.tagging.loudness_meter.clone()),
                cover_resolver: Some(downloads.tagging.download_cover_resolver.clone()),
                lyrics: Some(downloads.lyrics.clone()),
                last_fm: Some(integrations.last_fm.clone()),
            },
            SoulseekDownloadParts {
                slskd: downloads.soulseek_client.clone(),
                rejected_peers: stores.rejected_peers.clone(),
                verification: downloads.tagging.download_verification.clone(),
                youtube: stores.you_tube_resolver.clone(),
                id_registry: stores.external_id_registry.clone(),
                soulseek_link: Some(soulseek_link.clone()),
                lidarr_imports: Some(lidarr_import_handoff.clone()),
                lidarr_fetcher: Some(lidarr_track_fetcher.clone()),
            },
        );
        let download_service: Arc<dyn IDownloadService> = download_base;
        acquisition_activity.set_downloads(&download_service);

        let lidarr_hearts: Arc<dyn ILidarrHeartAcquisitionService> =
            Arc::new(LidarrHeartAcquisitionService::new(
                lidarr_client.clone(),
                music_metadata.clone(),
                clients.deezer_metadata.clone(),
                settings.clone(),
                navidrome.navidrome_identity.clone(),
                integrations.notifications.clone(),
                LidarrHeartExtras {
                    tracker: Some(acquisition_tracker.clone()),
                    music_brainz: Some(clients.music_brainz.clone()),
                    claims: Some(lidarr_album_claims.clone()),
                    imports: Some(lidarr_import_handoff.clone()),
                    ids: Some(stores.external_id_registry.clone()),
                    downloads: Some(download_service.clone()),
                    journal: Some(stores.library_action_journal.clone()),
                    ownership: Some(library_ownership.clone()),
                    upgrades: Some(upgrade_queue.clone()),
                },
            ));
        let heart_ownership = Arc::new(HeartOwnership::new(
            library_ownership.clone(),
            music_metadata.clone(),
            settings.clone(),
            Some(upgrade_queue.clone()),
            Some(upgrade_sources.clone()),
        ));
        let heart_acquisition_coordinator = HeartAcquisitionCoordinator::new(
            settings.clone(),
            track_acquisition_queue.clone(),
            download_service.clone(),
            lidarr_hearts.clone(),
            HeartCoordinatorExtras {
                tracker: Some(acquisition_tracker.clone()),
                id_registry: Some(stores.external_id_registry.clone()),
                soulseek: Some(soulseek_link),
                holds: Some(stores.soulseek_holds.clone()),
                owned: Some(heart_ownership.clone()),
                stars: Some(star_on_arrival.clone()),
            },
        );
        let external_search = Arc::new(ExternalSearchService::new(
            music_metadata,
            Some(integrations.last_fm.clone()),
            Some(settings.clone()),
        ));
        Acquisition {
            download_service,
            lidarr_client,
            lidarr_album_claims,
            lidarr_import_handoff,
            lidarr_track_fetcher,
            lidarr_hearts,
            track_acquisition_queue,
            acquisition_activity,
            acquisition_tracker,
            star_on_arrival,
            upgrade_queue,
            upgrade_sources,
            library_ownership,
            heart_ownership,
            heart_acquisition_coordinator,
            external_search,
        }
    }

    /// `AcquisitionWorker`, `SoulseekHoldResumer` and `CacheCleanupService` (`AddHostedService`).
    fn register_workers(
        &self,
        workers: &WorkerSupervisor,
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        notifications: &Arc<NotificationService>,
    ) {
        let worker = Arc::new(AcquisitionWorker::new(
            self.track_acquisition_queue.clone(),
            self.download_service.clone(),
            stores.external_id_registry.clone(),
            notifications.clone(),
            Some(stores.download_concurrency.clone()),
        ));
        workers.register("AcquisitionWorker", move |stopping| worker.clone().run(stopping));
        let resumer = Arc::new(SoulseekHoldResumer::new(
            stores.soulseek_holds.clone(),
            self.heart_acquisition_coordinator.clone(),
        ));
        workers.register("SoulseekHoldResumer", move |stopping| {
            resumer.clone().run(stopping)
        });
        let cleanup = Arc::new(CacheCleanupService::new(settings.clone()));
        workers.register("CacheCleanupService", move |stopping| {
            cleanup.clone().run(stopping)
        });
    }
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
    /// `create_download_directory`: whether to create the download directory, as the C#
    /// `LocalLibraryService` constructor did (not for handler tests). `more_validators` run
    /// after Subsonic's, in `Program.cs`'s registration order.
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        create_download_directory: bool,
        more_validators: Vec<Arc<dyn IStartupValidator>>,
    ) -> Self {
        let http = http_client_factory::default_client();
        let navidrome_identity = NavidromeIdentityService::new(Arc::clone(settings), http.clone());
        let library = LocalLibraryService::new(
            Arc::clone(settings),
            http.clone(),
            Arc::clone(&stores.external_id_registry),
            navidrome_identity.clone(),
        );
        if create_download_directory {
            library.create_download_directory();
        }
        let local_library: Arc<dyn ILocalLibraryService> = Arc::new(library);
        let mut validators: Vec<Arc<dyn IStartupValidator>> = vec![Arc::new(SubsonicStartupValidator::new(
            // IOptions<SubsonicSettings>: deliberately the URL Octo started with.
            settings.current().subsonic.url.clone(),
            http.clone(),
        ))];
        validators.extend(more_validators);
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
    subsonic_response_builder: Arc<SubsonicResponseBuilder>,
    library_action_journal: Arc<LibraryActionJournal>,
}

impl Stores {
    fn build(settings: &Arc<SettingsStore>, config_dir: &Path) -> Stores {
        let ttl_settings = settings.clone();
        let external_id_registry = Arc::new(ExternalIdRegistry::new(Some(
            config_dir.join("external-ids.json"),
        )));
        Stores {
            download_history: Arc::new(DownloadHistoryService::new(
                config_dir.join("downloads-history.json"),
            )),
            download_concurrency: Arc::new(DownloadConcurrency::new(settings.clone())),
            soulseek_holds: Arc::new(SoulseekHoldStore::new(Some(
                config_dir.join("soulseek-holds.json"),
            ))),
            you_tube_resolver: Arc::new(YouTubeResolver::new(settings)),
            subsonic_response_builder: Arc::new(new_subsonic_response_builder(
                external_id_registry.clone(),
                &settings.current().subsonic,
            )),
            external_id_registry,
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
            library_action_journal: Arc::new(LibraryActionJournal::with_path(Some(
                config_dir.join("library-actions.json"),
            ))),
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
        let journal = self.library_action_journal.clone();
        workers.register("LibraryActionJournal", move |token| {
            journal.clone().run_flusher(token)
        });
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

/// The lyrics services of task 3-C. `config_dir` None keeps the pins and the undo journal in
/// memory (handler tests).
struct Lyrics {
    choice_store: Arc<LyricsChoiceStore>,
    service: Arc<LyricsService>,
    choice_service: Arc<LyricsChoiceService>,
    sidecar_writer: Arc<LyricsSidecarWriter>,
    undo_journal: Arc<LyricsUndoJournal>,
}

impl Lyrics {
    fn build(settings: &Arc<SettingsStore>, config_dir: Option<&Path>) -> Lyrics {
        let sources = LyricsService::default_sources(
            lyrics_http::lyrics_http_client(),
            lyrics_http::kugou_http_client(),
        );
        let service = Arc::new(LyricsService::new(sources, settings.clone()));
        let choice_store = Arc::new(LyricsChoiceStore::new(
            config_dir.map(|dir| dir.join("lyrics-choices.json")),
        ));
        // The writer is given the library service (for the scan after writing inside songs)
        // with `set_library` once that is built.
        let sidecar_writer = Arc::new(LyricsSidecarWriter::new(service.clone(), settings.clone()));
        Lyrics {
            choice_service: Arc::new(LyricsChoiceService::new(service.clone(), choice_store.clone())),
            choice_store,
            service,
            sidecar_writer,
            undo_journal: Arc::new(LyricsUndoJournal::new(
                config_dir.map(|dir| dir.join("lyrics-undo.jsonl")),
            )),
        }
    }

    /// The writer is singleton AND hosted, the same instance both ways, so downloads enqueue
    /// into the worker the host is running.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let writer = self.sidecar_writer.clone();
        workers.register("LyricsSidecarWriter", move |stopping| {
            writer.clone().run(stopping)
        });
    }

    /// The library job of task 5-E, over the library service and the music folder, which exist
    /// only once Navidrome's services are built. `config_dir` None keeps the run in memory.
    fn library_job(
        &self,
        settings: &Arc<SettingsStore>,
        config_dir: Option<&Path>,
        navidrome: &NavidromeServices,
    ) -> LyricsLibraryJob {
        let store = Arc::new(LyricsLibraryStore::new(
            config_dir.map(|dir| dir.join("lyrics-library.json")),
        ));
        let resolver = navidrome.navidrome_song_path_resolver.clone();
        let worker = Arc::new(LyricsLibraryWorker::new(
            store.clone(),
            self.sidecar_writer.clone(),
            settings.clone(),
            navidrome.local_library.clone(),
            move || resolver.music_root(),
            self.undo_journal.clone(),
        ));
        LyricsLibraryJob { store, worker }
    }
}

/// `LyricsLibraryStore` and `LyricsLibraryWorker`.
struct LyricsLibraryJob {
    store: Arc<LyricsLibraryStore>,
    worker: Arc<LyricsLibraryWorker>,
}

impl LyricsLibraryJob {
    /// The store's flush timer (and the flush its `Dispose` did), and the worker, singleton AND
    /// hosted: the dashboard enqueues into the loop the host is running.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let store = self.store.clone();
        workers.register("LyricsLibraryStore", move |stopping| {
            store.clone().run_flusher(stopping)
        });
        let worker = self.worker.clone();
        workers.register("LyricsLibraryWorker", move |stopping| {
            worker.clone().run(stopping)
        });
    }
}

/// The Soulseek services of task 4-A, over the stores and clients they read.
struct SoulseekServices {
    client: SoulseekClient,
    link: Arc<SoulseekLink>,
    metadata: Arc<SoulseekMetadataService>,
    validator: Arc<dyn IStartupValidator>,
}

impl SoulseekServices {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        clients: &MetadataClients,
        last_fm: &Arc<LastFmService>,
    ) -> Self {
        // IOptions<SoulseekSettings>: the client deliberately keeps the address and login Octo
        // started with.
        let client = SoulseekClient::new(&settings.current().soulseek);
        SoulseekServices {
            link: Arc::new(SoulseekLink::new(client.clone(), Arc::clone(settings))),
            metadata: Arc::new(SoulseekMetadataService::new(
                Arc::clone(&stores.you_tube_resolver),
                Arc::clone(&stores.external_id_registry),
                Arc::clone(&clients.deezer_metadata),
                Arc::clone(&clients.cover_art_aggregator),
                Some(Arc::clone(last_fm) as Arc<dyn LastFmTrackLengths>),
            )),
            validator: Arc::new(SoulseekStartupValidator::new(settings, client.clone())),
            client,
        }
    }
}

/// Last.fm radio and the generated mixes (5-C).
struct Radio {
    state: Arc<LastFmRadioStateStore>,
    refresh_queue: Arc<LastFmRadioRefreshQueue>,
    sessions: Arc<LastFmRadioStreamSessionStore>,
    cache: Arc<LastFmRadioTrackCache>,
    recommendations: Arc<LastFmRadioRecommendationService>,
    streams: LastFmRadioStreamService,
    warmup: Arc<LastFmRadioWarmupService>,
    refresh_worker: Arc<LastFmRadioRefreshWorker>,
    generated_playlists: Arc<GeneratedPlaylistService>,
}

impl Radio {
    fn build(
        settings: &Arc<SettingsStore>,
        config_dir: &Path,
        stores: &Stores,
        navidrome: &NavidromeServices,
        integrations: &Integrations,
        music_metadata: Arc<dyn IMusicMetadataService>,
        downloads: Arc<dyn IDownloadService>,
    ) -> Radio {
        let state = Arc::new(LastFmRadioStateStore::new(
            config_dir.join("lastfm-radio-state.json"),
            settings.clone(),
            stores.external_id_registry.clone(),
        ));
        let refresh_queue = Arc::new(LastFmRadioRefreshQueue::new());
        let sessions = Arc::new(LastFmRadioStreamSessionStore::new());
        let cache = Arc::new(LastFmRadioTrackCache::new());
        let recommendations = Arc::new(LastFmRadioRecommendationService::new(
            integrations.last_fm.clone(),
            state.clone(),
            settings.clone(),
        ));
        let streams = LastFmRadioStreamService::new(LastFmRadioStreamServiceParts {
            state: state.clone(),
            settings: settings.clone(),
            library: navidrome.local_library.clone(),
            proxy: navidrome.subsonic_proxy.clone(),
            downloads,
            transcoder: Arc::new(FfmpegLastFmRadioAudioTranscoder),
            cache: cache.clone(),
            sessions: sessions.clone(),
            registry: stores.external_id_registry.clone(),
            metadata: music_metadata,
            queues: stores.radio_queues.clone(),
            refresh_queue: refresh_queue.clone(),
            tune_in: Arc::new(RandomRadioTuneInSelector),
            last_fm: Some(integrations.last_fm.clone()),
            listen_brainz: Some(integrations.listen_brainz.clone()),
            last_fm_scrobbles: Some(integrations.last_fm_scrobbles.clone()),
        });
        let warmup = Arc::new(LastFmRadioWarmupService::new(streams.clone(), state.clone()));
        let refresh_worker = Arc::new(LastFmRadioRefreshWorker::new(
            refresh_queue.clone(),
            recommendations.clone(),
            state.clone(),
            settings.clone(),
            Some(warmup.clone()),
        ));
        let generated_playlists = Arc::new(GeneratedPlaylistService::new(
            Some(config_dir.join("generated-playlists.json")),
            navidrome.subsonic_proxy.clone(),
            settings.clone(),
        ));
        Radio {
            state,
            refresh_queue,
            sessions,
            cache,
            recommendations,
            streams,
            warmup,
            refresh_worker,
            generated_playlists,
        }
    }

    /// `AddHostedService<LastFmRadioWarmupService>` (the singleton) and
    /// `AddHostedService<LastFmRadioRefreshWorker>`.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let warmup = self.warmup.clone();
        workers.register("LastFmRadioWarmupService", move |stopping| {
            warmup.clone().run(stopping)
        });
        let refresh = self.refresh_worker.clone();
        workers.register("LastFmRadioRefreshWorker", move |stopping| {
            refresh.clone().run(stopping)
        });
    }
}

/// Library actions (5-A): the quarantine, the executor and its two workers, over the journal
/// the stores hold.
struct LibraryActions {
    notice_queue: Arc<NoticeQueue>,
    quarantine: Arc<LibraryActionQuarantine>,
    executor: Arc<LibraryActionExecutor>,
    rating_worker: Arc<LibraryActionRatingWorker>,
    playlist_worker: Arc<LibraryActionPlaylistWorker>,
}

impl LibraryActions {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        acquisition: &Acquisition,
        soulseek_link: Arc<dyn ISoulseekLink>,
        spectrum: Arc<SpectrumAnalyzer>,
        notice_queue: Arc<NoticeQueue>,
    ) -> Self {
        let quarantine = Arc::new(LibraryActionQuarantine::new(settings.clone()));
        let executor = Arc::new(LibraryActionExecutor::new(LibraryActionExecutorParts {
            resolver: navidrome.navidrome_song_path_resolver.clone(),
            quarantine: quarantine.clone(),
            journal: stores.library_action_journal.clone(),
            library: navidrome.local_library.clone(),
            ids: stores.external_id_registry.clone(),
            rejected_peers: stores.rejected_peers.clone(),
            acquisitions: acquisition.track_acquisition_queue.clone(),
            settings: settings.clone(),
            notices: Some(notice_queue.clone()),
            // The one AddSingleton<SpectrumAnalyzer>() the downloads use too.
            spectrum: Some(spectrum),
            stars: Some(acquisition.star_on_arrival.clone()),
            soulseek_link: Some(soulseek_link),
            sources: Some(acquisition.upgrade_sources.clone()),
        }));
        let rating_worker = Arc::new(LibraryActionRatingWorker::new(
            executor.clone(),
            navidrome.subsonic_proxy.clone(),
            settings.clone(),
        ));
        let playlist_worker = Arc::new(LibraryActionPlaylistWorker::new(
            executor.clone(),
            stores.library_action_journal.clone(),
            quarantine.clone(),
            navidrome.navidrome_song_path_resolver.clone(),
            navidrome.navidrome_identity.clone(),
            navidrome.navidrome_playlist_api.clone(),
            settings.clone(),
        ));
        LibraryActions {
            notice_queue,
            quarantine,
            executor,
            rating_worker,
            playlist_worker,
        }
    }

    /// `AddHostedService<LibraryActionPlaylistWorker>()` and the rating worker, singleton AND
    /// hosted, the same instance both ways.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let playlists = self.playlist_worker.clone();
        workers.register("LibraryActionPlaylistWorker", move |stopping| {
            playlists.clone().run(stopping)
        });
        let ratings = self.rating_worker.clone();
        workers.register("LibraryActionRatingWorker", move |stopping| {
            ratings.clone().run(stopping)
        });
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
        let tagging = Tagging::build(&settings, &clients);
        let lyrics = Lyrics::build(&settings, Some(&config_dir));
        let workers = Arc::new(WorkerSupervisor::new());
        stores.register_workers(&workers);
        lyrics.register_workers(&workers);

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

        // The C# lookup's 15-second Timer, and its Dispose's last write.
        let itunes = Arc::clone(&clients.itunes_cover_art);
        workers.register("ITunesCoverArtLookup", move |stopping| {
            let itunes = Arc::clone(&itunes);
            async move { itunes.run_flush_loop(stopping).await }
        });
        let settings_writer = Arc::new(SettingsFileWriter::new(settings_path));
        let integrations = Integrations::build(&settings, &settings_writer, &clients.deezer_metadata);
        let soulseek = SoulseekServices::build(&settings, &stores, &clients, &integrations.last_fm);
        let navidrome = NavidromeServices::build(&settings, &stores, true, vec![soulseek.validator]);
        lyrics.sidecar_writer.set_library(navidrome.local_library.clone());
        let lyrics_library = lyrics.library_job(&settings, Some(&config_dir), &navidrome);
        lyrics_library.register_workers(&workers);
        // STUB(5-B): the real queue, with its file, replaces this when 5-B lands. Built before the
        // download service, which asks people through it (`NoticeQueue.AddReview`).
        let notice_queue = Arc::new(NoticeQueue::new());
        let acquisition = Acquisition::build(
            &settings,
            &stores,
            &navidrome,
            &integrations,
            &clients,
            soulseek.metadata.clone(),
            soulseek.link.clone(),
            DownloadDeps {
                tagging: &tagging,
                lyrics: &lyrics.sidecar_writer,
                soulseek_client: &soulseek.client,
                notice_queue: &notice_queue,
            },
        );
        acquisition.register_workers(&workers, &settings, &stores, &integrations.notifications);
        let library_actions = LibraryActions::build(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
            tagging.spectrum_analyzer.clone(),
            notice_queue.clone(),
        );
        library_actions.register_workers(&workers);
        let radio = Radio::build(
            &settings,
            &config_dir,
            &stores,
            &navidrome,
            &integrations,
            soulseek.metadata.clone(),
            acquisition.download_service.clone(),
        );
        radio.register_workers(&workers);
        let sync_catalog = Arc::new(SyncCatalogService::new(
            navidrome.subsonic_proxy.clone(),
            soulseek.metadata.clone(),
            stores.external_id_registry.clone(),
            settings.clone(),
            Clock::system(),
        ));
        let inner = AppInner {
            settings_writer,
            restart_tracker: Arc::new(restart_tracker),
            settings,
            workers,
            lyrics_choice_store: lyrics.choice_store,
            lyrics_service: lyrics.service,
            lyrics_choice_service: lyrics.choice_service,
            lyrics_sidecar_writer: lyrics.sidecar_writer,
            lyrics_undo_journal: lyrics.undo_journal,
            lyrics_library_store: lyrics_library.store,
            lyrics_library_worker: lyrics_library.worker,
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
            subsonic_response_builder: stores.subsonic_response_builder,
            soulseek_client: soulseek.client,
            soulseek_link: soulseek.link,
            music_metadata: soulseek.metadata,
            download_service: acquisition.download_service,
            lidarr_client: acquisition.lidarr_client,
            lidarr_album_claims: acquisition.lidarr_album_claims,
            lidarr_import_handoff: acquisition.lidarr_import_handoff,
            lidarr_track_fetcher: acquisition.lidarr_track_fetcher,
            lidarr_hearts: acquisition.lidarr_hearts,
            track_acquisition_queue: acquisition.track_acquisition_queue,
            acquisition_activity: acquisition.acquisition_activity,
            acquisition_tracker: acquisition.acquisition_tracker,
            star_on_arrival: acquisition.star_on_arrival,
            upgrade_queue: acquisition.upgrade_queue,
            upgrade_sources: acquisition.upgrade_sources,
            library_ownership: acquisition.library_ownership,
            heart_ownership: acquisition.heart_ownership,
            heart_acquisition_coordinator: acquisition.heart_acquisition_coordinator,
            external_search: acquisition.external_search,
            last_fm_radio_state: radio.state,
            last_fm_radio_refresh_queue: radio.refresh_queue,
            last_fm_radio_stream_sessions: radio.sessions,
            last_fm_radio_track_cache: radio.cache,
            last_fm_radio_recommendations: radio.recommendations,
            last_fm_radio_streams: radio.streams,
            last_fm_radio_warmup: radio.warmup,
            last_fm_radio_refresh_worker: radio.refresh_worker,
            generated_playlists: radio.generated_playlists,
            sync_catalog,
            library_action_journal: stores.library_action_journal,
            library_action_quarantine: library_actions.quarantine,
            library_action_executor: library_actions.executor,
            library_action_rating_worker: library_actions.rating_worker,
            notice_queue: library_actions.notice_queue,
            loudness_meter: tagging.loudness_meter,
            audio_fingerprinter: tagging.audio_fingerprinter,
            spectrum_analyzer: tagging.spectrum_analyzer,
            release_identifier: tagging.release_identifier,
            tag_preview: tagging.tag_preview,
            download_verification: tagging.download_verification,
            download_cover_resolver: tagging.download_cover_resolver,
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
        // The download service makes its music folder when it is built. Without a configured one
        // that is "./downloads" beside the test binary's working directory, so a handler test's
        // goes in its own temp folder instead.
        if store.raw("Library:DownloadPath").is_none() {
            store.set_raw(
                "Library:DownloadPath",
                Some(&config_dir.join("music").to_string_lossy()),
            );
        }
        let restart_tracker = Arc::new(RestartTracker::new(&store));
        let settings = Arc::new(store);
        let stores = Stores::build(&settings, &config_dir);
        // The clients point at the real hosts but ask nothing until a test calls them, and
        // the iTunes matches stay in memory.
        let clients = MetadataClients::build(&settings, None);
        let tagging = Tagging::build(&settings, &clients);
        let lyrics = Lyrics::build(&settings, None);
        // The Last.fm, ListenBrainz and notification clients point at the real hosts too, and
        // the default settings give none of them a key, token or URL to use.
        let settings_writer = Arc::new(SettingsFileWriter::new(config_dir.join("settings.json")));
        let integrations = Integrations::build(&settings, &settings_writer, &clients.deezer_metadata);
        let soulseek = SoulseekServices::build(&settings, &stores, &clients, &integrations.last_fm);
        let navidrome = NavidromeServices::build(&settings, &stores, false, vec![soulseek.validator]);
        lyrics.sidecar_writer.set_library(navidrome.local_library.clone());
        let lyrics_library = lyrics.library_job(&settings, None, &navidrome);
        // No workers: the acquisition worker, the resumer and the cache cleanup are not run.
        // STUB(5-B): the real queue, with its file, replaces this when 5-B lands. Built before the
        // download service, which asks people through it (`NoticeQueue.AddReview`).
        let notice_queue = Arc::new(NoticeQueue::new());
        let acquisition = Acquisition::build(
            &settings,
            &stores,
            &navidrome,
            &integrations,
            &clients,
            soulseek.metadata.clone(),
            soulseek.link.clone(),
            DownloadDeps {
                tagging: &tagging,
                lyrics: &lyrics.sidecar_writer,
                soulseek_client: &soulseek.client,
                notice_queue: &notice_queue,
            },
        );
        // Library actions keep their journal in the test's config directory; no worker is run.
        let library_actions = LibraryActions::build(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
            tagging.spectrum_analyzer.clone(),
            notice_queue.clone(),
        );
        // The radio's state files sit in the test's config directory; its workers are not run.
        let radio = Radio::build(
            &settings,
            &config_dir,
            &stores,
            &navidrome,
            &integrations,
            soulseek.metadata.clone(),
            acquisition.download_service.clone(),
        );
        let sync_catalog = Arc::new(SyncCatalogService::new(
            navidrome.subsonic_proxy.clone(),
            soulseek.metadata.clone(),
            stores.external_id_registry.clone(),
            settings.clone(),
            Clock::system(),
        ));
        let inner = AppInner {
            restart_tracker,
            settings,
            settings_writer,
            workers: Arc::new(WorkerSupervisor::new()),
            lyrics_choice_store: lyrics.choice_store,
            lyrics_service: lyrics.service,
            lyrics_choice_service: lyrics.choice_service,
            lyrics_sidecar_writer: lyrics.sidecar_writer,
            lyrics_undo_journal: lyrics.undo_journal,
            lyrics_library_store: lyrics_library.store,
            lyrics_library_worker: lyrics_library.worker,
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
            subsonic_response_builder: stores.subsonic_response_builder,
            soulseek_client: soulseek.client,
            soulseek_link: soulseek.link,
            music_metadata: soulseek.metadata,
            download_service: acquisition.download_service,
            lidarr_client: acquisition.lidarr_client,
            lidarr_album_claims: acquisition.lidarr_album_claims,
            lidarr_import_handoff: acquisition.lidarr_import_handoff,
            lidarr_track_fetcher: acquisition.lidarr_track_fetcher,
            lidarr_hearts: acquisition.lidarr_hearts,
            track_acquisition_queue: acquisition.track_acquisition_queue,
            acquisition_activity: acquisition.acquisition_activity,
            acquisition_tracker: acquisition.acquisition_tracker,
            star_on_arrival: acquisition.star_on_arrival,
            upgrade_queue: acquisition.upgrade_queue,
            upgrade_sources: acquisition.upgrade_sources,
            library_ownership: acquisition.library_ownership,
            heart_ownership: acquisition.heart_ownership,
            heart_acquisition_coordinator: acquisition.heart_acquisition_coordinator,
            external_search: acquisition.external_search,
            last_fm_radio_state: radio.state,
            last_fm_radio_refresh_queue: radio.refresh_queue,
            last_fm_radio_stream_sessions: radio.sessions,
            last_fm_radio_track_cache: radio.cache,
            last_fm_radio_recommendations: radio.recommendations,
            last_fm_radio_streams: radio.streams,
            last_fm_radio_warmup: radio.warmup,
            last_fm_radio_refresh_worker: radio.refresh_worker,
            generated_playlists: radio.generated_playlists,
            sync_catalog,
            library_action_journal: stores.library_action_journal,
            library_action_quarantine: library_actions.quarantine,
            library_action_executor: library_actions.executor,
            library_action_rating_worker: library_actions.rating_worker,
            notice_queue: library_actions.notice_queue,
            loudness_meter: tagging.loudness_meter,
            audio_fingerprinter: tagging.audio_fingerprinter,
            spectrum_analyzer: tagging.spectrum_analyzer,
            release_identifier: tagging.release_identifier,
            tag_preview: tagging.tag_preview,
            download_verification: tagging.download_verification,
            download_cover_resolver: tagging.download_cover_resolver,
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
        for name in [
            "ExternalIdRegistry",
            "RejectedPeerRegistry",
            "ReleaseCheck",
            "LyricsSidecarWriter",
            "LyricsLibraryStore",
            "LyricsLibraryWorker",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        assert!(
            AppState::for_tests(AppSettings::default())
                .workers
                .status()
                .is_empty()
        );
    }

    /// The acquisition pipeline (4-D): its three hosted services run as workers, the activity
    /// sees the download service, and StarOnArrival is already listening to the tracker.
    #[tokio::test]
    async fn the_acquisition_pipeline_is_wired_as_program_cs_registered_it() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in ["AcquisitionWorker", "SoulseekHoldResumer", "CacheCleanupService"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        use crate::services::common::IAcquisitionActivity;
        assert!(!state.acquisition_activity.is_busy());
        state
            .acquisition_tracker
            .begin("soulseek", "abc", None, Some("alice"), None, None, None);
        assert_eq!(state.acquisition_tracker.for_user("alice").len(), 1);
        assert_eq!(state.star_on_arrival.held(), 0);
        assert!(state.local_library.parse_song_id("ext-deezer-1").0);
    }

    /// SoulseekClient took `IOptions<SoulseekSettings>`: the address it started with stays,
    /// and the metadata service shares the registry the stores hold.
    #[tokio::test]
    async fn the_soulseek_client_keeps_the_address_it_started_with() {
        use crate::services::i_music_metadata_service::IMusicMetadataService;
        use octo_core::settings::SoulseekSettings;

        let state = AppState::for_tests(AppSettings {
            soulseek: SoulseekSettings {
                base_url: Some("http://slskd:5030/".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        state.settings.set(AppSettings::default());
        assert_eq!(state.soulseek_client.base_url(), "http://slskd:5030");

        let songs = state
            .music_metadata
            .search_songs_by_artist_title("Justice", "Genesis", 1, None)
            .await;
        assert!(state.external_id_registry.lookup(&songs[0].id).is_some());
    }

    /// Lidarr (4-E): the client reads the live settings, so with none saved nothing is asked and
    /// the track fetcher refuses before any lookup.
    #[tokio::test]
    async fn the_lidarr_services_refuse_until_lidarr_is_set_up() {
        use crate::services::lidarr::{LidarrError, LidarrTrackRequest};

        let state = AppState::for_tests(AppSettings::default());
        assert!(!state.lidarr_client.is_reachable().await);
        let request = LidarrTrackRequest {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            album: None,
            duration_seconds: None,
            lossless_only: true,
            original_path: None,
        };
        let refused = state
            .lidarr_track_fetcher
            .fetch(&request, "/nonexistent", &CancellationToken::new())
            .await;
        assert!(
            matches!(refused, Err(LidarrError::InvalidOperation(_))),
            "{refused:?}"
        );
        assert!(!state.lidarr_album_claims.upgrade_busy("anything"));
        assert!(state.lidarr_import_handoff.take("anything").is_none());
    }

    /// Last.fm radio (5-C): the warmup and the refresh worker run as workers, the refresh worker
    /// hands its rebuilds to the warmup, and the stores share the external id registry.
    #[tokio::test]
    async fn the_radio_is_wired_as_program_cs_registered_it() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in ["LastFmRadioWarmupService", "LastFmRadioRefreshWorker"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        assert!(state.last_fm_radio_state.known_users().is_empty());
        assert!(state.last_fm_radio_refresh_queue.enqueue("alice", None));
        assert!(state.last_fm_radio_warmup.queue_user("alice"));
        assert!(!state.last_fm_radio_warmup.queue_user("ALICE"));
        assert!(
            state
                .last_fm_radio_state
                .path()
                .ends_with("lastfm-radio-state.json")
        );
        assert!(state.generated_playlists.find("alice", "og1").is_none());
        let token = state
            .last_fm_radio_stream_sessions
            .issue("alice", "or1", [("u", "alice")], None);
        let session = state.last_fm_radio_stream_sessions.get(&token, None).unwrap();
        assert!(state.last_fm_radio_streams.resolve(&session).is_none());
    }

    /// 5-F: the sync catalog is one singleton; nothing is built or pinned until a walk asks.
    #[test]
    fn the_sync_catalog_is_a_singleton_with_nothing_built() {
        use crate::services::subsonic::SyncCatalogKind;
        let state = AppState::for_tests(AppSettings::default());
        assert!(state.sync_catalog.try_get_song("alice", "ph-1").is_none());
        assert!(
            state
                .sync_catalog
                .pinned_catalog("alice", SyncCatalogKind::Song)
                .is_none()
        );
    }

    /// Library actions (5-A): the journal's flusher and both workers run, the journal sits beside
    /// settings.json, and the Lidarr heart shares it.
    #[tokio::test]
    async fn library_actions_are_wired_as_program_cs_registered_them() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "LibraryActionJournal",
            "LibraryActionPlaylistWorker",
            "LibraryActionRatingWorker",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        assert!(
            state
                .library_action_journal
                .path()
                .is_some_and(|p| p.ends_with("library-actions.json"))
        );
        assert!(Arc::ptr_eq(
            state.library_action_executor.journal(),
            &state.library_action_journal
        ));
        let off = state
            .library_action_executor
            .apply(crate::services::library::LibraryActionRequest::new(
                octo_core::settings::LibraryAction::Delete,
                "nd-1",
                "alice",
            ))
            .await
            .expect("an outcome");
        assert_eq!(off.detail.as_deref(), Some("Library actions are off."));
        assert_eq!(state.library_action_rating_worker.pending(), 0);
        assert!(!state.upgrade_sources.ready());
    }

    #[test]
    fn stop_application_cancels_the_lifetime() {
        let state = AppState::for_tests(AppSettings::default());
        assert!(!state.lifetime.is_cancelled());
        state.stop_application();
        assert!(state.lifetime.is_cancelled());
    }
}
