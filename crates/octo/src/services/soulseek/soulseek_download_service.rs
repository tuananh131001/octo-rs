//! Port of `Services/Soulseek/SoulseekDownloadService.cs`: the service. The candidate matching
//! (the searches to run, the filename, length and version checks, the quality ranking) is pure
//! and lives in `octo_core::soulseek::soulseek_download_service`.
//!
//! The C# class was a subclass of `BaseDownloadService`. Here [`SoulseekDownloadService`] is the
//! base's [`DownloadBackend`]: the members the C# overrode, called back by the base with itself.
//! [`SoulseekDownloadService::build`] puts the two together, and the base is what the app holds
//! as its `IDownloadService`.
//!
//! The members the C# reached through `OptionalService<T>()` that the base does not carry (the
//! Soulseek link, Lidarr's import hand-off and track fetcher) are the `Option` fields of
//! [`SoulseekDownloadParts`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::TryStreamExt;
use octo_core::common::{dotnet, live_version};
use octo_core::fingerprint::{InconclusiveReason, VerificationResult, VerificationVerdict};
use octo_core::models::domain::{Album, Song};
use octo_core::notifications::{NotificationEvent, NotificationEventType};
use octo_core::settings::{DownloadSource, SoulseekSettings};
use octo_core::soulseek::soulseek_client::normalize_extension;
use octo_core::soulseek::soulseek_download_service::{
    BROWSE_TIMEOUT, PEER_FOLDERS_TO_BROWSE, adds_version, album_search_text, duration_plausible,
    filename_plausibly_matches_title, folder_of_file, from_live_folder, planned_queries, quality_penalty,
    size_sort_key, variant_penalty,
};
use octo_core::soulseek::{
    AlbumFolderPicker, AlbumTrack, RoutingKind, SearchProfile, SoulseekFileHit, SoulseekLinkState,
    SoulseekRouting, SoulseekTransferProgress, SoulseekTransferState,
};
use octo_media::audio::SpectrumReport;
use octo_media::tags::TagFile;
use parking_lot::Mutex;
use regex::Regex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::external_id_registry::{ExternalIdRegistry, SharedRouting};
use super::rejected_peer_registry::RejectedPeerRegistry;
use super::soulseek_client::{SoulseekClient, SoulseekClientError};
use super::soulseek_link::ISoulseekLink;
use super::soulseek_metadata_service::SoulseekMetadataService;
use crate::services::common::{
    AcquisitionState, BaseDownloadService, DownloadBackend, DownloadCore, DownloadServices,
    OperationCanceled, TrackDownload,
};
use crate::services::fingerprint::DownloadVerificationService;
use crate::services::i_download_service::DirectStreamInfo;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::lidarr::{ILidarrTrackFetcher, LidarrImport, LidarrImportHandoff, LidarrTrackRequest};
use crate::services::you_tube::YouTubeResolver;

/// `IncomingFolderName`: where the shim writes a download before Octo has decided its name. A dot
/// folder, which Navidrome's scanner skips (Scanner.IgnoreDotFolders, on by default), the same way
/// it skips the library-action quarantine.
pub const INCOMING_FOLDER_NAME: &str = ".octo-incoming";

/// slskd's default name for the folder a transfer is written into before it is moved.
pub const DEFAULT_INCOMPLETE_FOLDER_NAME: &str = "incomplete";

/// Search Soulseek, walk the top-N peers in quality order, first successful transfer wins.
/// ~30-50% of Soulseek peer requests are rejected (queue full / overwhelmed / banned), so trying
/// just the top hit fails too often.
///
/// Each attempt is bounded by Soulseek:DownloadTimeoutSeconds, so that setting is per peer and a
/// full walk can spend it MaxPeerAttempts times over.
pub const MAX_PEER_ATTEMPTS: usize = 5;

/// `InvalidOperationException`: a download that cannot be made as asked (no artist or title, no
/// download path, no Lidarr, audio YouTube could not decode). The library-action executor tries
/// the next source on it, as the C# caught the type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidOperationException(pub String);

/// `FileNotFoundException`: no copy found, or the copy found is gone. Reported as an `io::Error`
/// of kind `NotFound`, which is how the library-action executor recognises "no copy here".
fn file_not_found(message: String) -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, message).into()
}

/// A Soulseek call's failure, with a caller who gave up told apart as the base's
/// [`OperationCanceled`] (the C# `OperationCanceledException`).
fn client_error(error: SoulseekClientError) -> anyhow::Error {
    match error {
        SoulseekClientError::Cancelled => OperationCanceled.into(),
        other => anyhow::anyhow!("{other}"),
    }
}

/// The future, unless the caller gives up first: the C# passed its token to these calls, which
/// threw `OperationCanceledException` when it was cancelled.
async fn or_cancelled<T>(
    ct: &CancellationToken,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _ = ct.cancelled() => Err(OperationCanceled.into()),
        result = work => result,
    }
}

/// A song's file an album walk has queued already: the hit, the walk's job folder, the transfer
/// slskd gave it, and when it was queued, which says whether a rejected file is ours.
#[derive(Debug, Clone)]
pub struct PreparedTransfer {
    pub hit: SoulseekFileHit,
    pub job_dir: String,
    pub transfer_id: Option<String>,
    pub queued_utc: DateTime<Utc>,
}

/// A likely transcode held back while other peers are tried, and what it took to get it.
struct TranscodedReserve {
    path: String,
    hit: SoulseekFileHit,
    attempt: usize,
    verdict: VerificationResult,
    spectrum: SpectrumReport,
    started_utc: DateTime<Utc>,
}

/// What to do with a likely transcode, given the one already held back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveChoice {
    Hold,
    AlreadyHeld,
    DiscardNew,
}

/// What the C# constructor took beyond the base's own services.
pub struct SoulseekDownloadParts {
    pub slskd: SoulseekClient,
    pub rejected_peers: Arc<RejectedPeerRegistry>,
    pub verification: Arc<DownloadVerificationService>,
    pub youtube: Arc<YouTubeResolver>,
    pub id_registry: Arc<ExternalIdRegistry>,
    /// `OptionalService<ISoulseekLink>()`.
    pub soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    /// `OptionalService<LidarrImportHandoff>()`.
    pub lidarr_imports: Option<Arc<LidarrImportHandoff>>,
    /// `OptionalService<ILidarrTrackFetcher>()`.
    pub lidarr_fetcher: Option<Arc<dyn ILidarrTrackFetcher>>,
}

/// Hybrid download service:
///   - `get_direct_stream`  -> instant lossy preview via YouTube (yt-dlp)
///   - `download_track`     -> permanent FLAC fetch via slskd. Runs when the user
///     stars a track, and in Permanent mode when one is
///     played. Soulseek is searched here on demand using
///     the encoded artist+title.
pub struct SoulseekDownloadService {
    slskd: SoulseekClient,
    rejected_peers: Arc<RejectedPeerRegistry>,
    verification: Arc<DownloadVerificationService>,
    /// `IOptions<SoulseekSettings>.Value`: deliberately captured at construction, as the C#
    /// did. The search ceilings, the preferred extension, the minimum size and the per-peer
    /// timeout are restart-only settings.
    settings: SoulseekSettings,
    youtube: Arc<YouTubeResolver>,
    id_registry: Arc<ExternalIdRegistry>,
    soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    lidarr_imports: Option<Arc<LidarrImportHandoff>>,
    lidarr_fetcher: Option<Arc<dyn ILidarrTrackFetcher>>,

    // Set once slskd has said where its incomplete folder is; until then each download asks again.
    excluded_folders: Mutex<Option<Vec<String>>>,

    // Songs whose file an album walk has queued already, by external id, taken by the song's own
    // download. A walk removes whatever it left here when it ends.
    prepared: Mutex<HashMap<String, PreparedTransfer>>,

    // slskd's downloads directory as Octo sees it, once slskd has said; empty when it is not a
    // path here. And roots learned from a job folder found somewhere else.
    slskd_downloads: Mutex<Option<String>>,
    learned_roots: Mutex<Vec<String>>,

    // The batches an album walk queued, by the walk's prepared track ids, so the walk's end can let
    // go of what it did not use.
    album_batches: Mutex<HashMap<String, (String, String)>>,
}

impl SoulseekDownloadService {
    /// `soulseek_settings` is the `IOptions` value, captured here.
    pub fn new(soulseek_settings: SoulseekSettings, parts: SoulseekDownloadParts) -> Self {
        SoulseekDownloadService {
            slskd: parts.slskd,
            rejected_peers: parts.rejected_peers,
            verification: parts.verification,
            settings: soulseek_settings,
            youtube: parts.youtube,
            id_registry: parts.id_registry,
            soulseek_link: parts.soulseek_link,
            lidarr_imports: parts.lidarr_imports,
            lidarr_fetcher: parts.lidarr_fetcher,
            excluded_folders: Mutex::new(None),
            prepared: Mutex::new(HashMap::new()),
            slskd_downloads: Mutex::new(None),
            learned_roots: Mutex::new(Vec::new()),
            album_batches: Mutex::new(HashMap::new()),
        }
    }

    /// The download service as the app registers it (`AddSingleton<IDownloadService,
    /// SoulseekDownloadService>()`): the base over this backend. The Soulseek settings are read
    /// from the store once, here.
    pub fn build(
        core: DownloadCore,
        services: DownloadServices,
        parts: SoulseekDownloadParts,
    ) -> (Arc<BaseDownloadService>, Arc<SoulseekDownloadService>) {
        let backend = Arc::new(Self::new(core.settings.current().soulseek.clone(), parts));
        let base = BaseDownloadService::new(core, services, Arc::clone(&backend) as Arc<dyn DownloadBackend>);
        (base, backend)
    }

    fn routing_for(&self, external_id: &str) -> Option<SharedRouting> {
        self.id_registry.lookup(external_id).or_else(|| {
            SoulseekMetadataService::try_decode_external_id(Some(external_id)).map(SharedRouting::new)
        })
    }

    // =========================================================================
    // Lidarr
    // =========================================================================

    /// A file a Lidarr heart brought in, taken into a job folder and held to the checks a Soulseek
    /// download meets. AcoustID naming another recording, or a live take nobody asked for, throws
    /// the file away: Lidarr has no second copy, so the heart's next source tries instead. A
    /// FLAC made from an MP3 is kept and marked, as Soulseek keeps its last resort.
    async fn adopt_lidarr_import(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        import: LidarrImport,
    ) -> anyhow::Result<String> {
        let (artist, title) = names(routing);
        download.fetched_from = Some("Lidarr".to_string());
        if import.quiet {
            download.muted = true;
        }
        if !file_exists(&import.path) {
            return Err(file_not_found(format!(
                "Lidarr's file for '{artist} - {title}' is gone: {}",
                import.path
            )));
        }

        let job_dir = combine(
            &combine(&combine(&base.download_path(), INCOMING_FOLDER_NAME), "lidarr"),
            &new_guid(),
        );
        std::fs::create_dir_all(&job_dir)?;
        let local = combine(&job_dir, file_name(&import.path));
        if std::fs::rename(&import.path, &local).is_err() {
            // Another volume: copied, then the import removed.
            std::fs::copy(&import.path, &local)?;
            std::fs::remove_file(&import.path)?;
        }
        BaseDownloadService::try_remove_empty_parents(
            directory_name(&import.path).as_deref(),
            &base.download_path(),
        );

        let song = &mut download.song;
        if let (Some(provider), Some(id)) = (
            song.external_provider.clone().filter(|p| !p.is_empty()),
            song.external_id.clone().filter(|i| !i.is_empty()),
        ) {
            base.track(|t| t.stage(&provider, &id, AcquisitionState::Verifying, Some("Lidarr"), None));
        }
        let verdict = self
            .verification
            .verify(
                &local,
                routing.artist.as_deref(),
                routing.title.as_deref(),
                song.isrc.as_deref().or(routing.isrc.as_deref()),
                !live_version::requested(routing.title.as_deref(), routing.album.as_deref()),
            )
            .await;
        if verdict.verdict == VerificationVerdict::Mismatch {
            warn!(
                "Lidarr brought {} for '{artist} - {title}' (AcoustID score {}); discarding it",
                verdict.describe(),
                percent(verdict.score)
            );
            // Swept with the job folders when it cannot be removed now.
            let _ = std::fs::remove_file(&local);
            return Err(file_not_found(format!(
                "AcoustID identified Lidarr's file as {}",
                verdict.describe()
            )));
        }
        let spectrum = self
            .verification
            .check_lossless(&local, routing.artist.as_deref(), routing.title.as_deref())
            .await;
        if spectrum.is_likely_lossy() {
            warn!(
                "Lidarr's copy of '{artist} - {title}' is {}; keeping it, marked as a transcode",
                spectrum.describe()
            );
            song.transcoded_from = spectrum.estimate.clone();
        }
        info!("Took Lidarr's import of '{artist} - {title}' into the pipeline: {local}");
        Ok(local)
    }

    /// One song through Lidarr, copied into a job folder under the incoming dot folder, which no
    /// scan looks at. From there it is identified, checked and swapped in like any download.
    async fn download_via_lidarr(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        lossless_only: bool,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        // A heart's album that Lidarr already brought in: the file is here, and only needs taking.
        if let Some(external_id) = download.song.external_id.clone().filter(|id| !id.is_empty())
            && let Some(import) = self.lidarr_imports.as_ref().and_then(|h| h.take(&external_id))
        {
            return self.adopt_lidarr_import(base, routing, download, import).await;
        }

        let Some(fetcher) = &self.lidarr_fetcher else {
            return Err(InvalidOperationException("Lidarr is not available on this server.".into()).into());
        };
        let song = &download.song;
        if let (Some(provider), Some(id)) = (
            song.external_provider.as_deref().filter(|p| !p.is_empty()),
            song.external_id.as_deref().filter(|i| !i.is_empty()),
        ) {
            base.track(|t| {
                t.stage(
                    provider,
                    id,
                    AcquisitionState::Searching,
                    Some("Lidarr"),
                    Some("Lidarr is searching the album"),
                )
            });
        }
        let job_dir = combine(
            &combine(&combine(&base.download_path(), INCOMING_FOLDER_NAME), "lidarr"),
            &new_guid(),
        );
        let request = LidarrTrackRequest {
            artist: routing.artist.clone().unwrap_or_default(),
            title: routing.title.clone().unwrap_or_default(),
            album: routing.album.clone(),
            duration_seconds: routing.duration.or(song.duration),
            lossless_only,
            original_path: download.replacing_path.clone(),
        };
        download.fetched_from = Some("Lidarr".to_string());
        Ok(fetcher.fetch(&request, &job_dir, ct).await?)
    }

    // =========================================================================
    // YouTube
    // =========================================================================

    // Lossy MP3 via the yt-dlp shim's /download. The shim writes <dest>.mp3 into the staging
    // folder with clean tags and a cover; the base's placement moves it once the tags are settled.
    async fn download_via_you_tube(
        &self,
        base: &BaseDownloadService,
        routing: &SharedRouting,
        download: &mut TrackDownload,
        announce_start: bool,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        let download_path = base.download_path();
        if download_path.is_empty() {
            return Err(InvalidOperationException("DownloadPath is not configured".into()).into());
        }

        let provider = base.provider_name().to_string();
        let track_key = download.song.external_id.clone().unwrap_or_default();
        base.track(|t| {
            t.stage(
                &provider,
                &track_key,
                AcquisitionState::Searching,
                Some("YouTube"),
                None,
            )
        });

        let r = routing.snapshot();
        let (artist, title) = names(&r);
        let mut video_id = r.you_tube_id.clone().filter(|v| !v.is_empty());
        if video_id.is_none() {
            let hit = self
                .youtube
                .search(&format!("{artist} {title}"), r.duration, false)
                .await;
            video_id = hit.map(|h| h.video_id).filter(|v| !v.is_empty());
            if let Some(found) = &video_id {
                routing.lock().you_tube_id = Some(found.clone());
            }
        }
        let Some(video_id) = video_id else {
            return Err(file_not_found(format!(
                "No YouTube match for '{artist} - {title}'"
            )));
        };

        if !download.suppress_notify && announce_start {
            base.notifications().notify(NotificationEvent {
                artist: r.artist.clone(),
                title: r.title.clone(),
                album: r.album.clone(),
                source: Some("YouTube".into()),
                format: Some("MP3".into()),
                duration_seconds: r.duration,
                ..NotificationEvent::new(NotificationEventType::DownloadStarted)
            });
        }

        // Staged, not written into the library. Writing straight to the layout path let the shim
        // overwrite a different file that happened to share the name before anything could
        // protect it. Extension left empty: the shim appends .mp3 itself.
        let incoming = combine(&download_path, INCOMING_FOLDER_NAME);
        sweep_incoming(&incoming);
        let dest_without_ext = combine(&incoming, &format!("{video_id}-{}", new_guid()));

        // The shim answers only once the file is written, so there is nothing to count here.
        base.track(|t| t.transfer(&provider, &track_key, None, None, None, Some("YouTube")));
        let path = or_cancelled(ct, async {
            Ok(self
                .youtube
                .download(
                    &video_id,
                    &dest_without_ext,
                    r.artist.as_deref(),
                    r.title.as_deref(),
                )
                .await)
        })
        .await?;
        let Some(path) = path.filter(|p| !p.is_empty() && file_exists(p)) else {
            return Err(file_not_found(format!(
                "YouTube MP3 download failed for '{artist} - {title}'"
            )));
        };

        info!("YouTube MP3 download complete: {path}");

        // Identification only. YouTube has no second candidate to fall back to, so a
        // disagreement is something to ask a person about (the Review playlist), never a reason
        // to throw the song away.
        base.track(|t| t.stage(&provider, &track_key, AcquisitionState::Verifying, None, None));
        let song = &mut download.song;
        let mut verdict = self
            .verification
            .verify(
                &path,
                r.artist.as_deref(),
                r.title.as_deref(),
                song.isrc.as_deref().or(r.isrc.as_deref()),
                false,
            )
            .await;
        if verdict.verdict == VerificationVerdict::Mismatch {
            if verdict.r#match.is_none() && verdict.matched_title.as_deref().is_none_or(str::is_empty) {
                // No decodable audio at all: a broken file, not a question.
                let _ = std::fs::remove_file(&path);
                return Err(InvalidOperationException(format!(
                    "YouTube delivered no decodable audio for '{artist} - {title}'"
                ))
                .into());
            }
            warn!(
                "AcoustID says the YouTube file for '{artist} - {title}' is {}; keeping it and asking about it",
                verdict.describe()
            );
            verdict.verdict = VerificationVerdict::Inconclusive;
            verdict.reason = InconclusiveReason::SourceDisagreed;
        } else {
            verdict.apply_tags_to(song);
        }
        song.verification = Some(Box::new(verdict));
        Ok(path)
    }

    // =========================================================================
    // Job folders
    // =========================================================================

    /// Where a job folder can be, in order: slskd's own downloads directory when that path exists
    /// here too, the download path, /music, and any root learned from a job folder found elsewhere.
    async fn job_roots(&self, base: &BaseDownloadService) -> Vec<String> {
        let known = self.slskd_downloads.lock().is_some();
        if !known && let Some(reported) = self.slskd.get_downloads_directory().await {
            let seen = if Path::new(&reported).is_dir() {
                reported
            } else {
                String::new()
            };
            *self.slskd_downloads.lock() = Some(seen);
        }
        let mut roots: Vec<String> = Vec::new();
        let mut add = |root: Option<String>| {
            if let Some(root) = root.filter(|r| !r.is_empty())
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        };
        add(self.slskd_downloads.lock().clone());
        add(Some(base.download_path()));
        add(Some("/music".to_string()));
        for learned in self.learned_roots.lock().clone() {
            add(Some(learned));
        }
        roots
    }

    /// slskd says the transfer finished, and its file is not in the job folder where Octo looks.
    /// Either slskd's downloads directory is somewhere else under the music folder, and the job
    /// folder is found and its root learned, so downloads stay parallel and exact; or slskd ignored
    /// the folder (a subdirectory pattern of {}), and the file is found by name the old way and
    /// downloads go one at a time from here on.
    fn find_outside_job(
        &self,
        base: &BaseDownloadService,
        job_dir: &str,
        hit: &SoulseekFileHit,
        require_exact_size: bool,
        excluded: &[String],
    ) -> Option<String> {
        let job = job_dir.rsplit('/').next().unwrap_or(job_dir);
        let tail = combine(&combine(INCOMING_FOLDER_NAME, "slskd"), job);
        let download_path = base.download_path();
        let mut roots: Vec<&str> = Vec::new();
        for root in [download_path.as_str(), "/music"] {
            if !root.is_empty() && !roots.contains(&root) {
                roots.push(root);
            }
        }
        for root in roots {
            let attempt = (|| -> std::io::Result<Option<String>> {
                if !Path::new(root).is_dir() {
                    return Ok(None);
                }
                let Some(folder) = find_directory(root, job, |d| d.ends_with(&tail))? else {
                    return Ok(None);
                };
                let learned = folder[..folder.len() - tail.len()]
                    .trim_end_matches(['/', '\\'])
                    .to_string();
                if let Some(found) = resolve_in_job(
                    std::slice::from_ref(&learned),
                    job_dir,
                    &hit.filename,
                    hit.size,
                    require_exact_size,
                ) {
                    {
                        let mut learned_roots = self.learned_roots.lock();
                        if !learned_roots.contains(&learned) {
                            learned_roots.push(learned.clone());
                        }
                    }
                    info!(
                        "slskd's downloads directory is {learned} as Octo sees it; job folders are looked for there too"
                    );
                    if let Some(concurrency) = base.concurrency() {
                        concurrency.prove();
                    }
                    return Ok(Some(found));
                }
                Ok(None)
            })();
            match attempt {
                Ok(Some(found)) => return Some(found),
                Ok(None) => {}
                Err(e) => debug!("Looking for job folder {job} under {root} failed: {e}"),
            }
        }

        let elsewhere = self.resolve_landed(base, &hit.filename, hit.size, require_exact_size, excluded)?;
        if let Some(concurrency) = base.concurrency() {
            concurrency.refuse("slskd put a download outside the folder Octo asked for");
        }
        Some(elsewhere)
    }

    /// Removes job folders a placed or discarded download left empty, after half an hour so an album
    /// batch between two files is left alone, and any job folder a day old. Only inside Octo's own
    /// slskd job folder; nothing else is touched.
    fn sweep_job_folders(&self, base: &BaseDownloadService) {
        let download_path = base.download_path();
        if download_path.is_empty() {
            return;
        }
        let jobs = combine(&combine(&download_path, INCOMING_FOLDER_NAME), "slskd");
        let sweep = || -> std::io::Result<()> {
            if !Path::new(&jobs).is_dir() {
                return Ok(());
            }
            for entry in std::fs::read_dir(&jobs)? {
                let dir = entry?.path();
                if !dir.is_dir() {
                    continue;
                }
                let empty = std::fs::read_dir(&dir)?.next().is_none();
                let age = age_of(&std::fs::metadata(&dir)?.modified()?);
                let young = if empty {
                    age < Duration::from_secs(30 * 60)
                } else {
                    age < Duration::from_secs(24 * 3600)
                };
                if young {
                    continue;
                }
                if empty {
                    std::fs::remove_dir(&dir)?;
                } else {
                    std::fs::remove_dir_all(&dir)?;
                    info!("Removed a stale download folder {}", dir.display());
                }
            }
            Ok(())
        };
        if let Err(e) = sweep() {
            debug!("Could not sweep {jobs}: {e}");
        }
    }

    /// Drops every song still waiting on one album batch and cancels its transfers.
    async fn abandon_album_batch(&self, job_dir: &str) {
        let waiting: Vec<String> = self
            .prepared
            .lock()
            .iter()
            .filter(|(_, p)| p.job_dir == job_dir)
            .map(|(id, _)| id.clone())
            .collect();
        if waiting.is_empty() {
            return;
        }
        info!(
            "The album folder's peer did not send; {} more songs search on their own",
            waiting.len()
        );
        for id in waiting {
            let Some(dropped) = self.prepared.lock().remove(&id) else {
                continue;
            };
            // The cancel logs its own failure.
            self.slskd
                .cancel_transfer(
                    &dropped.hit.username,
                    &dropped.hit.filename,
                    dropped.transfer_id.as_deref(),
                )
                .await;
        }
    }

    // =========================================================================
    // Soulseek
    // =========================================================================

    async fn excluded_folders(&self) -> Vec<String> {
        if let Some(known) = self.excluded_folders.lock().clone() {
            return known;
        }
        let configured = self.slskd.get_incomplete_directory().await;
        let names = excluded_folder_names(configured.as_deref());
        // Kept only when slskd answered, so a failed read is tried again on the next download
        // instead of settling on the default for the life of the process.
        if configured.is_some() {
            *self.excluded_folders.lock() = Some(names.clone());
        }
        names
    }

    fn resolve_landed(
        &self,
        base: &BaseDownloadService,
        remote_filename: &str,
        expected_size: i64,
        require_exact_size: bool,
        excluded: &[String],
    ) -> Option<String> {
        let mut roots: Vec<String> = Vec::new();
        let download_path = base.download_path();
        if !download_path.is_empty() {
            roots.push(download_path);
        }
        if !roots.iter().any(|r| r == "/music") {
            roots.push("/music".to_string());
        }
        resolve_local_path(
            remote_filename,
            expected_size,
            require_exact_size,
            &roots,
            excluded,
            Some(&|root: &str, message: &str| debug!("Path scan failed under {root}: {message}")),
        )
    }

    fn rank_candidates(
        &self,
        hits: &[SoulseekFileHit],
        title: &str,
        expected_duration: Option<i32>,
        strict: bool,
        album: Option<&str>,
    ) -> Vec<SoulseekFileHit> {
        let wanted = normalize_extension(Some(&self.settings.preferred_extension), "");
        let remembers = self.verification.remembers_rejections();
        let mut found: Vec<&SoulseekFileHit> = hits
            .iter()
            // First because it is the cheapest filter and the only one backed by evidence
            // from a completed transfer: these exact files were downloaded, inspected and
            // found to be the wrong recording. Offering them again spends a whole transfer
            // to reach the same verdict.
            .filter(|h| candidate_allowed(h, Some(&self.rejected_peers), remembers))
            .filter(|h| dotnet::eq_ignore_case(&h.extension, &wanted))
            .filter(|h| h.size >= self.settings.min_file_size_bytes)
            .filter(|h| filename_plausibly_matches_title(&h.filename, title, strict))
            .filter(|h| duration_plausible(h.length, expected_duration, strict))
            .filter(|h| !adds_version(&h.filename, title))
            .filter(|h| !from_live_folder(&h.filename, title, album))
            .collect();
        // An unnamed bracketed addition sorts last rather than being dropped: it may be a
        // different take ("Angel (Angel Dust)"), or only a peer's own label. Stable, as LINQ's
        // OrderBy is.
        found.sort_by_cached_key(|h| {
            (
                variant_penalty(&h.filename, title),
                quality_penalty(h),
                h.queue_length.unwrap_or(i32::MAX),
                std::cmp::Reverse(h.upload_speed.unwrap_or(0)),
                size_sort_key(h.size, Some(&wanted)),
            )
        });
        found.into_iter().take(MAX_PEER_ATTEMPTS).cloned().collect()
    }

    /// A search that found the song only lossy (an MP3 where FLAC is preferred) looks in the
    /// folders those copies sit in: a peer often shares an album in both formats, side by side,
    /// and only one format answered the search (#70). The best few peers are asked for that one
    /// folder, and what they list is ranked exactly like search hits, so title, length, version
    /// and live folder all count, and AcoustID and the spectrum still check the download.
    async fn beside_lossy_copies(
        &self,
        hits: &[SoulseekFileHit],
        routing: &SoulseekRouting,
    ) -> anyhow::Result<Vec<SoulseekFileHit>> {
        let wanted = normalize_extension(Some(&self.settings.preferred_extension), "");
        let title = routing.title.clone().unwrap_or_default();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut lossy: Vec<&SoulseekFileHit> = hits
            .iter()
            .filter(|h| !dotnet::eq_ignore_case(&h.extension, &wanted))
            .filter(|h| !folder_of_file(&h.filename).is_empty())
            .filter(|h| filename_plausibly_matches_title(&h.filename, &title, false))
            .filter(|h| duration_plausible(h.length, routing.duration, false))
            .filter(|h| !adds_version(&h.filename, &title))
            .filter(|h| !from_live_folder(&h.filename, &title, routing.album.as_deref()))
            // One per peer folder, the first of each.
            .filter(|h| seen.insert((h.username.clone(), folder_of_file(&h.filename).to_string())))
            .collect();
        lossy.sort_by_key(|h| {
            (
                std::cmp::Reverse(h.has_free_upload_slot == Some(true)),
                h.queue_length.unwrap_or(i32::MAX),
                std::cmp::Reverse(h.upload_speed.unwrap_or(0)),
            )
        });
        lossy.truncate(PEER_FOLDERS_TO_BROWSE);
        if lossy.is_empty() {
            return Ok(Vec::new());
        }

        let mut listed: Vec<SoulseekFileHit> = Vec::new();
        // One at a time: slskd runs one peer operation at a time anyway.
        for hit in &lossy {
            listed.extend(
                self.slskd
                    .browse_folder(hit, folder_of_file(&hit.filename), BROWSE_TIMEOUT)
                    .await?,
            );
        }
        let found = self.rank_candidates(&listed, &title, routing.duration, false, routing.album.as_deref());
        info!(
            "Soulseek: no {} of '{} - {title}' in the search; looked in {} peers' folders beside their lossy copy and found {}",
            dotnet::to_upper_invariant(&wanted),
            routing.artist.as_deref().unwrap_or(""),
            lossy.len(),
            found.len()
        );
        Ok(found)
    }

    /// The song's searches in order, ranked; the first that finds a usable candidate ends them.
    /// `pass_over` is a file already tried (the album folder's copy), never offered again.
    async fn search_ranked(
        &self,
        routing: &SoulseekRouting,
        profile: &SearchProfile,
        pass_over: Option<&SoulseekFileHit>,
        ct: &CancellationToken,
    ) -> anyhow::Result<Vec<SoulseekFileHit>> {
        let (artist, title) = names(routing);
        let queries = planned_queries(&title, &artist, routing.album.as_deref(), routing.duration);
        let not_passed_over = |h: &SoulseekFileHit| {
            pass_over.is_none_or(|p| h.username != p.username || h.filename != p.filename)
        };
        let mut hits: Vec<SoulseekFileHit> = Vec::new();
        let mut found: Vec<SoulseekFileHit> = Vec::new();
        // Every query's answers, for the folder look below.
        let mut every_hit: Vec<SoulseekFileHit> = Vec::new();
        for (index, (query, strict)) in queries.iter().enumerate() {
            let text = query.text();
            if index == 0 {
                info!("Soulseek search-for-star: '{text}'");
            } else {
                info!("Soulseek query returned no usable hits; retrying with '{text}'");
            }
            // Ranked once, on the whole search: slskd hands over a search's answers only when it
            // ends, so there is nothing to stop early on.
            hits = self
                .slskd
                .search(&text, profile, ct)
                .await
                .map_err(client_error)?;
            every_hit.extend(hits.iter().cloned());
            found = self
                .rank_candidates(&hits, &title, routing.duration, *strict, routing.album.as_deref())
                .into_iter()
                .filter(not_passed_over)
                .collect();
            if !found.is_empty() {
                break;
            }
        }
        if found.is_empty() {
            found = self
                .beside_lossy_copies(&every_hit, routing)
                .await?
                .into_iter()
                .filter(not_passed_over)
                .collect();
        }

        // Logged here, once per song, rather than inside rank_candidates. This is the line that
        // explains a track that used to download and now does not.
        if self.verification.remembers_rejections() {
            let denied = hits
                .iter()
                .filter(|h| {
                    self.rejected_peers
                        .is_denied(Some(&h.username), Some(&h.filename))
                })
                .count();
            if denied > 0 {
                info!(
                    "Soulseek: {denied} of {} hits for '{artist} - {title}' were downloaded before and \
                     rejected as the wrong recording, so they are skipped. Use 'Forget rejected peers' on \
                     the Soulseek admin page if that is wrong.",
                    hits.len()
                );
            }
        }
        Ok(found)
    }

    /// Remember a peer and file we downloaded and rejected, so rank_candidates never offers it
    /// again. Before this, a rejection was deleted and the fact thrown away: the next star
    /// re-ran the same search, ranked the same peer first for the same reasons, and paid for
    /// the same wrong file again.
    ///
    /// Gated on VerifyDownloads so that with the flag off, which is the default, nothing about
    /// the existing behaviour changes, including the memory.
    fn deny_candidate(&self, hit: &SoulseekFileHit, routing: &SoulseekRouting, reason: &str) {
        if !self.verification.remembers_rejections() {
            return;
        }
        let (artist, title) = names(routing);
        self.rejected_peers.deny(
            Some(&hit.username),
            Some(&hit.filename),
            reason,
            &format!("{artist} - {title}"),
        );
    }

    // Lossless FLAC via Soulseek/slskd: walk the top-N peers in quality order,
    // first successful transfer wins.
    async fn download_via_soulseek(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        profile: &SearchProfile,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        let (artist, title) = names(routing);
        let primary_query = planned_queries(&title, &artist, routing.album.as_deref(), routing.duration)[0]
            .0
            .text();
        let provider = base.provider_name().to_string();
        let track_key = download.song.external_id.clone().unwrap_or_default();
        base.track(|t| {
            t.stage(
                &provider,
                &track_key,
                AcquisitionState::Searching,
                Some("Soulseek"),
                None,
            )
        });
        self.sweep_job_folders(base);

        // An album walk may have queued this song's file already, from one peer's folder of the
        // album. That file is tried first, without a search; the search runs only if it fails.
        let prepared = self.prepared.lock().remove(&track_key);
        let mut searched = prepared.is_none();
        // Whether the album folder's file for this song arrived at all, whatever the checks said.
        let mut prepared_arrived = false;
        let mut ranked = match &prepared {
            None => self.search_ranked(routing, profile, None, ct).await?,
            Some(p) => vec![p.hit.clone()],
        };
        // Whether `ranked` is the album folder's copy (the C# compared the hit by reference).
        let mut ranked_is_prepared = prepared.is_some();

        if ranked.is_empty() {
            return Err(file_not_found(format!(
                "No Soulseek {} found for '{artist} - {title}'",
                dotnet::to_upper_invariant(&self.settings.preferred_extension)
            )));
        }

        info!(
            "Soulseek: {} candidate peers for '{primary_query}', trying in order",
            ranked.len()
        );

        // Read before the first attempt: every resolve in the loop must already know which
        // folder holds slskd's unfinished copies (#69).
        let excluded = self.excluded_folders().await;

        let mut last_error: Option<String> = None;
        let mut start_announced = false;

        // A file that claims to be lossless and whose spectrum says it was made from a lossy
        // one. It is still the right song, so it is held back rather than thrown away: a later
        // peer's genuine copy replaces it, and when no peer has one it is what this download
        // delivers, never a reason to fail the song or fall back to YouTube.
        let mut reserve: Option<TranscodedReserve> = None;

        let isrc = download.song.isrc.clone().or(routing.isrc.clone());
        let refuse_live = !live_version::requested(routing.title.as_deref(), routing.album.as_deref());

        // Every peer tried, across the album folder's copy and the search after it.
        let mut tried = 0;
        loop {
            for (index, hit) in ranked.iter().enumerate() {
                let attempt_idx = index + 1;
                tried += 1;
                info!(
                    "Soulseek attempt {attempt_idx}/{}: {} -> {} (queue={}, speed={})",
                    ranked.len(),
                    hit.username,
                    hit.filename,
                    display_option(hit.queue_length),
                    display_option(hit.upload_speed)
                );

                // Each attempt lands in a folder of its own, so finding its file never rests on the
                // file's name, and a download beside it can never claim it. A file an album walk queued
                // is already on its way into the walk's folder.
                let from_album = ranked_is_prepared;
                let album_copy = prepared.as_ref().filter(|_| from_album);
                let job_dir = album_copy.map_or_else(new_job_dir, |p| p.job_dir.clone());
                let mut transfer_id = album_copy.and_then(|p| p.transfer_id.clone());
                let mut batched = from_album;

                // Used to decide whether a rejected file is ours to delete.
                let attempt_started_utc = album_copy.map_or_else(Utc::now, |p| p.queued_utc);

                if !from_album {
                    let enqueued = or_cancelled(ct, async {
                        let batch = self
                            .slskd
                            .enqueue_batch(&hit.username, &[(hit.filename.clone(), hit.size)], &job_dir)
                            .await
                            .map_err(client_error)?;
                        if batch.supported {
                            let Some(id) = batch.transfer_ids.get(&hit.filename) else {
                                anyhow::bail!(
                                    "{}",
                                    batch
                                        .failures
                                        .iter()
                                        .map(|(_, message)| message.as_str())
                                        .find(|m| !m.is_empty())
                                        .unwrap_or("slskd queued nothing")
                                );
                            };
                            Ok(Some(id.clone()))
                        } else {
                            self.slskd
                                .enqueue_download(&hit.username, &hit.filename, hit.size)
                                .await
                                .map_err(client_error)?;
                            Ok(None)
                        }
                    })
                    .await;
                    match enqueued {
                        Ok(Some(id)) => {
                            transfer_id = Some(id);
                            batched = true;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(
                                "Soulseek enqueue failed for {} ({e}); trying next peer",
                                hit.username
                            );
                            last_error = Some(e.to_string());
                            continue;
                        }
                    }
                }

                // A peer took it, but it can sit in that peer's queue for a while, so this still
                // reads as searching until bytes move. A retry on the next peer starts from nothing.
                base.track(|t| {
                    t.stage(
                        &provider,
                        &track_key,
                        AcquisitionState::Searching,
                        Some("Soulseek"),
                        None,
                    )
                });

                // Announced only after a peer actually accepted the transfer -- firing
                // before the loop would claim a start that five straight rejections later
                // never happened. Once per track: a retry on the next peer is the same
                // download, not a new one. SizeBytes is this candidate's advertised size;
                // DownloadCompleted carries the real file's.
                if !download.suppress_notify && !start_announced {
                    start_announced = true;
                    base.notifications().notify(NotificationEvent {
                        artist: routing.artist.clone(),
                        title: routing.title.clone(),
                        album: routing.album.clone(),
                        source: Some("Soulseek".into()),
                        format: Some(dotnet::to_upper_invariant(&self.settings.preferred_extension)),
                        size_bytes: Some(hit.size),
                        duration_seconds: routing.duration,
                        ..NotificationEvent::new(NotificationEventType::DownloadStarted)
                    });
                }

                // Cancelling the WAIT must never cancel the TRANSFER. slskd already
                // accepted the enqueue and keeps going on its own, so letting this
                // throw straight out of the loop is what used to lose a finished
                // download: the disk check, the move, the registration and the
                // rescan were all skipped while the file quietly landed anyway.
                //
                // slskd's size is the real one once the peer answers; the search's is
                // what the peer advertised, kept for polls that leave it out.
                let on_progress = |p: &SoulseekTransferProgress| {
                    if p.is_moving() {
                        base.track(|t| {
                            t.transfer(
                                &provider,
                                &track_key,
                                p.bytes_transferred,
                                Some(p.size.unwrap_or(hit.size)),
                                p.percent_complete,
                                Some("Soulseek"),
                            )
                        });
                    }
                };
                let waited = self
                    .slskd
                    .wait_for_completion(
                        &hit.username,
                        &hit.filename,
                        Some(self.settings.download_timeout_seconds),
                        ct,
                        Some(&on_progress),
                        transfer_id.as_deref(),
                    )
                    .await;
                let (state, wait_error) = match waited {
                    Ok(state) => (Some(state), None),
                    Err(e) => (None, Some(client_error(e))),
                };

                // An slskd HTTP timeout and a client disconnect both surface as
                // cancellations, so the token is the only reliable way to
                // tell "the caller left" from "slskd was slow".
                let caller_gave_up = ct.is_cancelled();

                // Regardless of slskd's reported final state, the authoritative
                // signal is the filesystem. slskd sometimes drops successful
                // transfers from /api/v0/transfers/downloads/<user> between our
                // polls, so we'd see Errored/timeout even though the file landed
                // on disk a second ago. Check disk first; fall back to "this
                // peer failed, try the next" only when the file truly isn't there.
                //
                // The usual 64KB size tolerance absorbs slskd's own size drift, but
                // an interrupted transfer is far more likely to be genuinely
                // truncated, so demand an exact match before promoting one.
                //
                // The check re-polls the disk for a bounded window: slskd reports
                // Succeeded BEFORE moving the file out of its incomplete directory,
                // and on bind mounts that move is a copy that can take seconds.
                let max_wait = if state == Some(SoulseekTransferState::Succeeded) {
                    Duration::from_secs(15)
                } else {
                    Duration::from_secs(5)
                };
                let job_roots = if batched {
                    self.job_roots(base).await
                } else {
                    Vec::new()
                };
                // Where this attempt's file is now, asked again after a check that took seconds.
                let find_own_file = || {
                    if batched {
                        resolve_in_job(&job_roots, &job_dir, &hit.filename, hit.size, caller_gave_up)
                    } else {
                        self.resolve_landed(base, &hit.filename, hit.size, caller_gave_up, &excluded)
                    }
                };
                let mut local_path = retry_resolve(find_own_file, max_wait, Duration::from_secs(1), ct).await;
                if batched && local_path.is_none() && state == Some(SoulseekTransferState::Succeeded) {
                    local_path = self.find_outside_job(base, &job_dir, hit, caller_gave_up, &excluded);
                } else if batched
                    && local_path.is_some()
                    && let Some(concurrency) = base.concurrency()
                {
                    concurrency.prove();
                }
                if from_album && local_path.is_some() {
                    prepared_arrived = true;
                }
                let local_path = local_path.filter(|p| !p.is_empty());
                if let (Some(path), Some(held)) = (&local_path, &reserve)
                    && same_path(&held.path, path)
                {
                    // The resolver matches on leaf name and size, so a peer offering the same rip as
                    // the copy held back can resolve to that very file. It is not a new copy, and
                    // every check below would either repeat itself or, worse, delete the only one.
                    info!(
                        "Soulseek attempt {attempt_idx} resolved to the copy already held back, not a new file; advancing"
                    );
                    last_error = Some("the file found is the copy already held back".into());
                    if caller_gave_up {
                        break;
                    }
                    continue;
                }

                if let Some(mut local_path) = local_path {
                    base.track(|t| t.stage(&provider, &track_key, AcquisitionState::Verifying, None, None));

                    // Last line of defence, and the only one that inspects the actual audio.
                    // A peer can advertise a length it does not deliver, and the tagger runs
                    // straight after this and would stamp the RIGHT title onto the wrong
                    // recording, leaving a library that looks correct and plays wrong.
                    let (matches, actual_secs) = downloaded_duration_matches(&local_path, routing.duration);
                    if !matches {
                        let expected = display_option(routing.duration);
                        warn!(
                            "Soulseek attempt {attempt_idx} delivered the wrong recording for '{artist} - {title}': \
                             {actual_secs}s against an expected {expected}s; discarding and advancing"
                        );
                        discard_rejected_download(&local_path, attempt_started_utc);
                        self.deny_candidate(
                            hit,
                            routing,
                            &format!("delivered {actual_secs}s for a {expected}s track"),
                        );
                        last_error = Some(format!(
                            "peer delivered a {actual_secs}s file for a {expected}s track"
                        ));
                        continue;
                    }

                    // Same job as the duration check, one layer deeper: that one proves the file
                    // is the right LENGTH, and a cover, a live take or an unrelated song of the
                    // same runtime all survive it. This asks what the audio actually IS.
                    //
                    // Second on purpose. The check above reads a header; this one spawns a
                    // process and makes a network call, and neither is worth spending on a file
                    // already known to be wrong.
                    let mut verdict = self
                        .verification
                        .verify(
                            &local_path,
                            routing.artist.as_deref(),
                            routing.title.as_deref(),
                            isrc.as_deref(),
                            refuse_live,
                        )
                        .await;
                    // fpcalc reports a file that is not there as undecodable audio, which is a Mismatch.
                    // If the file moved during the check, find it and ask again instead of blaming the
                    // peer for slskd's own move.
                    if verdict.verdict == VerificationVerdict::Mismatch
                        && !file_exists(&local_path)
                        && let Some(moved_to) = find_own_file()
                    {
                        local_path = moved_to;
                        verdict = self
                            .verification
                            .verify(
                                &local_path,
                                routing.artist.as_deref(),
                                routing.title.as_deref(),
                                isrc.as_deref(),
                                refuse_live,
                            )
                            .await;
                    }
                    if verdict.verdict == VerificationVerdict::Mismatch {
                        if !file_exists(&local_path) {
                            // Nothing was judged, so nothing is held against the peer: a deny-list entry
                            // lasts weeks and this one would be wrong.
                            warn!(
                                "Soulseek attempt {attempt_idx}: {local_path} disappeared while it was being identified; \
                                 advancing without blaming {}",
                                hit.username
                            );
                            last_error =
                                Some("the downloaded file disappeared while it was being identified".into());
                            // Like the held-back copy: with the caller gone, no other peer is tried.
                            if caller_gave_up {
                                break;
                            }
                            continue;
                        }
                        warn!(
                            "Soulseek attempt {attempt_idx} delivered {} for a request of '{artist} - {title}' \
                             (AcoustID score {}); discarding, remembering the peer and advancing",
                            verdict.describe(),
                            percent(verdict.score)
                        );
                        discard_rejected_download(&local_path, attempt_started_utc);
                        self.deny_candidate(hit, routing, &verdict.deny_reason);
                        last_error = Some(format!("AcoustID identified the file as {}", verdict.describe()));
                        continue;
                    }

                    // Last, and the only check that never rejects: the right song made from an MP3 is
                    // still the right song. It decides only whether another peer's copy is worth a
                    // try, which is why it runs after the checks that can throw a file away.
                    let spectrum = self
                        .verification
                        .check_lossless(&local_path, routing.artist.as_deref(), routing.title.as_deref())
                        .await;
                    if spectrum.is_likely_lossy() {
                        match weigh_transcode(
                            reserve.as_ref().map(|r| r.path.as_str()),
                            reserve.as_ref().and_then(|r| r.spectrum.cutoff_hz),
                            &local_path,
                            spectrum.cutoff_hz,
                        ) {
                            ReserveChoice::AlreadyHeld => {
                                info!(
                                    "Soulseek attempt {attempt_idx} resolved to the copy already held back; advancing"
                                );
                            }
                            ReserveChoice::Hold => {
                                if let Some(held) = &reserve {
                                    discard_rejected_download(&held.path, held.started_utc);
                                }
                                reserve = Some(TranscodedReserve {
                                    path: local_path.clone(),
                                    hit: hit.clone(),
                                    attempt: attempt_idx,
                                    verdict,
                                    spectrum: spectrum.clone(),
                                    started_utc: attempt_started_utc,
                                });
                            }
                            ReserveChoice::DiscardNew => {
                                discard_rejected_download(&local_path, attempt_started_utc);
                            }
                        }

                        warn!(
                            "Soulseek attempt {attempt_idx} for '{artist} - {title}' is {}; {}",
                            spectrum.describe(),
                            if caller_gave_up {
                                "the caller has left, so no other copy is tried"
                            } else {
                                "holding it back and trying the next lossless copy"
                            }
                        );
                        last_error = Some(format!("the file is {}", spectrum.describe()));
                        if caller_gave_up {
                            break;
                        }
                        continue;
                    }

                    // Checked again at the last moment, and before the held-back copy is let go. The
                    // checks above take seconds, and a path that stopped existing in that time would be
                    // placed, tagged and recorded as a download with nothing on disk (#69).
                    if !file_matches(&local_path, hit.size, caller_gave_up) {
                        let Some(found_again) = find_own_file() else {
                            warn!(
                                "Soulseek attempt {attempt_idx}: {local_path} is gone since it was checked; advancing"
                            );
                            last_error =
                                Some("the downloaded file disappeared before it could be kept".into());
                            // Like the held-back copy: with the caller gone, no other peer is tried.
                            if caller_gave_up {
                                break;
                            }
                            continue;
                        };
                        local_path = found_again;
                    }

                    if let Some(held) = &reserve
                        && !same_path(&held.path, &local_path)
                    {
                        info!(
                            "Soulseek attempt {attempt_idx} is a genuine copy; it replaces the transcoded one from attempt {}",
                            held.attempt
                        );
                        discard_rejected_download(&held.path, held.started_utc);
                    }

                    // The file stays where slskd put it; the base's placement moves it once it knows
                    // the album and the credit it will be filed under.
                    info!(
                        "Soulseek download complete (attempt {attempt_idx}, slskd state={}{}): {local_path}",
                        state.map_or_else(|| "interrupted".to_string(), |s| format!("{s:?}")),
                        if caller_gave_up {
                            ", caller had already left"
                        } else {
                            ""
                        }
                    );
                    return Ok(accept(&mut download.song, local_path, hit, verdict, None));
                }

                if let Some(wait_error) = wait_error {
                    // Nothing on disk and the caller is gone: no later peer attempt
                    // has anywhere to be delivered, so stop instead of burning the
                    // rest of the list. A copy held back is still delivered.
                    if caller_gave_up {
                        if reserve.is_some() {
                            break;
                        }
                        return Err(wait_error);
                    }
                    warn!("Soulseek attempt {attempt_idx} wait failed ({wait_error}); advancing");
                    last_error = Some(wait_error.to_string());
                    continue;
                }

                let state_text = state.map(|s| format!("{s:?}")).unwrap_or_default();
                info!(
                    "Soulseek attempt {attempt_idx} failed (state={state_text}, no file on disk), advancing"
                );
                last_error = Some(format!(
                    "transfer ended in state {state_text} with no resulting file"
                ));
            }

            // The album folder's copy did not work out: now search for this song like any other.
            if searched || ct.is_cancelled() {
                break;
            }
            let album_copy = prepared
                .as_ref()
                .expect("a song not searched yet has a prepared file");
            // A peer that never sent this file will not send the rest either: let the walk's other
            // songs search now instead of each waiting out the same silence.
            if !prepared_arrived {
                self.abandon_album_batch(&album_copy.job_dir).await;
            }
            searched = true;
            ranked = self
                .search_ranked(routing, profile, Some(&album_copy.hit), ct)
                .await?;
            ranked_is_prepared = false;
            if ranked.is_empty() {
                break;
            }
            info!(
                "Soulseek: the album folder's copy of '{artist} - {title}' did not work out; {} other peers to try",
                ranked.len()
            );
        }

        if let Some(kept) = reserve {
            if file_exists(&kept.path) {
                // Kept, not failed: the song asked for is on disk, only not in the quality its
                // extension claims. Written down so it can be found and upgraded later.
                warn!(
                    "Soulseek: no genuine lossless copy of '{artist} - {title}' among {} candidates; keeping attempt {} \
                     from {}, which is {}: {}",
                    ranked.len(),
                    kept.attempt,
                    kept.hit.username,
                    kept.spectrum.describe(),
                    kept.path
                );
                let estimate = kept.spectrum.estimate.clone();
                return Ok(accept(
                    &mut download.song,
                    kept.path,
                    &kept.hit,
                    kept.verdict,
                    estimate,
                ));
            }
            warn!(
                "Soulseek: the copy held back from attempt {} is no longer at {}",
                kept.attempt, kept.path
            );
            last_error = Some("the copy held back is no longer on disk".into());
        }

        anyhow::bail!(
            "All {tried} Soulseek peer attempts failed for '{artist} - {title}'. Last error: {}. \
             If slskd shows these transfers as Completed, slskd's downloads directory is not the directory Octo watches ({}); \
             set SLSKD_DOWNLOADS_DIR=/music on the slskd container (see issue #17).",
            last_error.unwrap_or_default(),
            base.download_path()
        )
    }
}

/// Records the ids of a confirmed match, and its name too when tagging from MusicBrainz is on.
/// Reads and writes the Song, never the routing. It reaches the tagger and the placement because
/// the base passes ONE Song through the download, the identification, placement and tagging.
///
/// Also writes down who delivered the file. It is the only chance: after the transfer ends
/// nothing else in Octo remembers, and "Wrong song" needs it to blacklist the peer rather than
/// re-rolling the same search.
fn accept(
    song: &mut Song,
    path: String,
    source: &SoulseekFileHit,
    verdict: VerificationResult,
    transcoded_from: Option<String>,
) -> String {
    verdict.apply_tags_to(song);
    song.verification = Some(Box::new(verdict));
    song.source_peer = Some(source.username.clone());
    song.source_file = Some(source.filename.clone());
    song.transcoded_from = transcoded_from;
    path
}

#[async_trait]
impl DownloadBackend for SoulseekDownloadService {
    fn provider_name(&self) -> &str {
        SoulseekMetadataService::PROVIDER_NAME
    }

    async fn is_available(&self, _base: &BaseDownloadService) -> bool {
        self.slskd.is_reachable().await
    }

    // Octo's album ids ARE the external id, so this is identity plus a kind check that
    // stops a song or artist id being walked as if it were an album.
    fn extract_external_id_from_album_id(&self, album_id: &str) -> Option<String> {
        self.id_registry
            .lookup(album_id)
            .filter(|routing| routing.lock().kind == RoutingKind::Album)
            .map(|_| album_id.to_string())
    }

    /// Restore an album track's routing if the registry evicted it mid-download.
    /// The fields here MUST match what SoulseekMetadataService's get_album registered
    /// (YouTubeId left None, the Song's own Duration) or this hashes to a different id and
    /// fails to restore anything. Routings are mutated in place elsewhere, so rebuild from
    /// the Song, which still carries the values used at registration time.
    fn ensure_routing_registered(&self, track: &Song) {
        let Some(external_id) = track.external_id.as_deref().filter(|id| !id.is_empty()) else {
            return;
        };
        if self.id_registry.lookup(external_id).is_some() {
            return;
        }

        self.id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(track.artist.clone()),
            title: Some(track.title.clone()),
            album: Some(track.album.clone()),
            duration: track.duration,
            track: track.track,
            disc_number: track.disc_number,
            total_tracks: track.total_tracks,
            isrc: track.isrc.clone(),
            ..Default::default()
        });
    }

    // =========================================================================
    // Streaming path (every play of an unowned radio track)
    // =========================================================================
    async fn get_direct_stream(
        &self,
        _base: &BaseDownloadService,
        external_provider: &str,
        external_id: &str,
        range_header: Option<&str>,
        _cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        if !dotnet::eq_ignore_case(external_provider, SoulseekMetadataService::PROVIDER_NAME) {
            return Ok(None);
        }

        let Some(routing) = self.routing_for(external_id) else {
            return Ok(None);
        };

        let r = routing.snapshot();
        let mut video_id = r.you_tube_id.clone().filter(|v| !v.is_empty());
        if video_id.is_none() && r.has_artist_title() {
            let (artist, title) = names(&r);
            let hit = self
                .youtube
                .search(&format!("{artist} {title}"), r.duration, false)
                .await;
            video_id = hit.map(|h| h.video_id).filter(|v| !v.is_empty());
            // Cache back on the routing so a second click on the same placeholder
            // skips the yt-dlp ytsearch1: round trip — that 3-8s saving is the
            // difference between Arpeggi (~10s HTTP timeout) playing the song or
            // canceling and falling back to a local one. The routing is shared via
            // the registry, so this change is visible to every subsequent stream
            // request for this id.
            if let Some(found) = &video_id {
                routing.lock().you_tube_id = Some(found.clone());
            }
        }
        let Some(video_id) = video_id else {
            return Ok(None);
        };

        let Some(opened) = self.youtube.open_stream(&video_id, range_header).await else {
            warn!("yt-dlp shim failed to open stream for vid={video_id}");
            return Ok(None);
        };

        info!(
            "YouTube preview '{} - {}' (vid={video_id}, status={}, {} bytes{})",
            r.artist.as_deref().unwrap_or(""),
            r.title.as_deref().unwrap_or(""),
            opened.status_code,
            opened.content_length.map(|l| l.to_string()).unwrap_or_default(),
            opened
                .content_range
                .as_deref()
                .map(|range| format!(", range={range}"))
                .unwrap_or_default()
        );

        // The response owns the shim's connection: dropping the stream closes it, which is what
        // the C# OwningStream did on Dispose.
        Ok(Some(DirectStreamInfo {
            audio_stream: Box::pin(opened.response.bytes_stream().map_err(std::io::Error::other)),
            content_type: opened.content_type,
            content_length: opened.content_length,
            quality: Some("youtube-m4a".to_string()),
            status_code: opened.status_code,
            content_range: opened.content_range,
        }))
    }

    // =========================================================================
    // Permanent download path (a star, or a play while in Permanent mode)
    // =========================================================================
    async fn download_track(
        &self,
        base: &BaseDownloadService,
        download: &mut TrackDownload,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String> {
        let external_id = download.song.external_id.clone().unwrap_or_default();
        let routing = self
            .routing_for(&external_id)
            .filter(|routing| routing.lock().has_artist_title());
        let Some(routing) = routing else {
            return Err(InvalidOperationException(format!(
                "Cannot download '{} - {}': missing artist/title in external id",
                download.song.artist, download.song.title
            ))
            .into());
        };
        let profile = if download.upgrade_search {
            SearchProfile::upgrade(&self.settings)
        } else {
            SearchProfile::interactive(&self.settings)
        };

        // Only ever asked for by name: a library action's replacement. DownloadSource set to
        // Lidarr means hearts go to Lidarr, and every other download still uses Soulseek below.
        if download.source_override == Some(DownloadSource::Lidarr) {
            let snapshot = routing.snapshot();
            let lossless_only = download.upgrade_search;
            return self
                .download_via_lidarr(base, &snapshot, download, lossless_only, cancellation_token)
                .await;
        }

        // DownloadOnStar decides WHETHER to download; DownloadSource decides FROM WHERE.
        let source = download
            .source_override
            .unwrap_or(base.subsonic_settings().download_source);
        match source {
            DownloadSource::YouTube => {
                self.download_via_you_tube(base, &routing, download, true, cancellation_token)
                    .await
            }
            DownloadSource::SoulseekThenYouTube => {
                // The filter matters: a cancelled token means nobody is waiting for
                // this any more, so falling back would start a second download only
                // to have it throw on the same token.
                let snapshot = routing.snapshot();
                match self
                    .download_via_soulseek(base, &snapshot, download, &profile, cancellation_token)
                    .await
                {
                    Ok(path) => Ok(path),
                    Err(e) if !cancellation_token.is_cancelled() => {
                        warn!("Soulseek download failed ({e}); falling back to YouTube MP3");
                        if !download.suppress_notify {
                            base.notifications().notify(NotificationEvent {
                                artist: snapshot.artist.clone(),
                                title: snapshot.title.clone(),
                                album: snapshot.album.clone(),
                                source: Some("YouTube".into()),
                                format: Some("MP3".into()),
                                detail: Some(e.to_string()),
                                ..NotificationEvent::new(NotificationEventType::LosslessFallback)
                            });
                        }
                        // announce_start false: the fallback event above already announces
                        // the MP3, and one gesture should never ping twice.
                        self.download_via_you_tube(base, &routing, download, false, cancellation_token)
                            .await
                    }
                    Err(e) => Err(e),
                }
            }
            _ => {
                let snapshot = routing.snapshot();
                self.download_via_soulseek(base, &snapshot, download, &profile, cancellation_token)
                    .await
            }
        }
    }

    /// One search for the whole album, and the one peer folder that covers most of it queued in one
    /// batch into one job folder. Each covered track then takes its file without a search. Nothing
    /// is prepared (the walk is song by song, as before) when album folders are off, the source has
    /// no Soulseek in it, slskd is not logged in, there are fewer than three tracks, too few lengths
    /// are known, no folder covers half the album, or slskd takes no batches.
    async fn prepare_album(
        &self,
        base: &BaseDownloadService,
        album: &Album,
        tracks: &[Song],
        source: Option<DownloadSource>,
        cancellation_token: &CancellationToken,
    ) -> Vec<String> {
        let none = Vec::new();
        let settings = base.current_soulseek_settings();
        if !settings.album_folders || tracks.len() < 3 {
            return none;
        }
        if !matches!(
            source.unwrap_or(base.subsonic_settings().download_source),
            DownloadSource::Soulseek | DownloadSource::SoulseekThenYouTube
        ) {
            return none;
        }
        if let Some(link) = &self.soulseek_link
            && link.read(false).await.map(|r| r.link) == Some(SoulseekLinkState::NotLoggedIn)
        {
            return none;
        }
        let Some(text) = album_search_text(Some(&album.artist), Some(&album.title)) else {
            return none;
        };

        let wanted: Vec<AlbumTrack> = tracks
            .iter()
            .filter(|t| {
                t.external_id.as_deref().is_some_and(|id| !id.is_empty()) && !dotnet::is_blank(&t.title)
            })
            .map(|t| {
                AlbumTrack::new(
                    t.external_id.clone().unwrap_or_default(),
                    t.title.clone(),
                    t.duration,
                    t.track,
                )
            })
            .collect();
        info!("Soulseek album search: '{text}' for {} tracks", wanted.len());
        let attempt = async {
            let hits = self
                .slskd
                .search(&text, &SearchProfile::album(&self.settings), cancellation_token)
                .await
                .map_err(client_error)?;
            let wanted_ext = normalize_extension(Some(&self.settings.preferred_extension), "");
            let remembers = self.verification.remembers_rejections();
            let choice = AlbumFolderPicker::choose(&hits, &wanted, |hit| {
                candidate_allowed(hit, Some(&self.rejected_peers), remembers)
                    && dotnet::eq_ignore_case(&hit.extension, &wanted_ext)
                    && hit.size >= self.settings.min_file_size_bytes
                    // A studio album is never taken from a live album's folder.
                    && !from_live_folder(&hit.filename, "", Some(&album.title))
            });
            let Some(choice) = choice else {
                info!(
                    "Album '{}': no one folder covers enough of it; searching song by song",
                    album.title
                );
                return anyhow::Ok(Vec::new());
            };

            let job_dir = new_job_dir();
            let files: Vec<(String, i64)> = choice
                .files
                .iter()
                .map(|(_, file)| (file.filename.clone(), file.size))
                .collect();
            let batch = or_cancelled(cancellation_token, async {
                self.slskd
                    .enqueue_batch(&choice.username, &files, &job_dir)
                    .await
                    .map_err(client_error)
            })
            .await?;
            if !batch.supported {
                return Ok(Vec::new());
            }

            let queued = Utc::now();
            let mut prepared = Vec::new();
            for (track, file) in &choice.files {
                let Some(transfer_id) = batch.transfer_ids.get(&file.filename) else {
                    continue;
                };
                self.prepared.lock().insert(
                    track.external_id.clone(),
                    PreparedTransfer {
                        hit: file.clone(),
                        job_dir: job_dir.clone(),
                        transfer_id: Some(transfer_id.clone()),
                        queued_utc: queued,
                    },
                );
                self.album_batches.lock().insert(
                    track.external_id.clone(),
                    (choice.username.clone(), job_dir.clone()),
                );
                prepared.push(track.external_id.clone());
            }
            info!(
                "Album '{}': {} of {} tracks from {}'s folder '{}' in one batch",
                album.title,
                prepared.len(),
                wanted.len(),
                choice.username,
                choice.folder
            );
            Ok(prepared)
        };
        match attempt.await {
            Ok(prepared) => prepared,
            // The C# let a cancellation through to the walk; the trait answers a list, so a walk
            // whose caller left goes on with nothing prepared, and each of its downloads stops on
            // the same token.
            Err(e) if e.downcast_ref::<OperationCanceled>().is_some() => none,
            Err(e) => {
                warn!(
                    "Album '{}': the album search failed ({e}); searching song by song",
                    album.title
                );
                none
            }
        }
    }

    /// The walk is over: cancel in slskd every prepared transfer no track took, so a folder that did
    /// not work out stops downloading files nobody will use, and remove the walk's job folder once
    /// it is empty.
    async fn finish_album(&self, base: &BaseDownloadService, prepared: &[String]) {
        let mut folders: Vec<String> = Vec::new();
        for id in prepared {
            if let Some((_, job_dir)) = self.album_batches.lock().remove(id)
                && !folders.contains(&job_dir)
            {
                folders.push(job_dir);
            }
            let Some(unused) = self.prepared.lock().remove(id) else {
                continue;
            };
            // The cancel logs its own failure.
            self.slskd
                .cancel_transfer(
                    &unused.hit.username,
                    &unused.hit.filename,
                    unused.transfer_id.as_deref(),
                )
                .await;
        }
        for job_dir in folders {
            for root in self.job_roots(base).await {
                let folder = combine(&root, &job_dir);
                let path = Path::new(&folder);
                let empty = path.is_dir() && std::fs::read_dir(path).is_ok_and(|mut e| e.next().is_none());
                if empty && let Err(e) = std::fs::remove_dir(path) {
                    debug!("Could not remove album job folder {folder}: {e}");
                }
            }
        }
    }
}

// =========================================================================
// The statics: candidates, job folders and finding a landed file
// =========================================================================

/// Is this candidate still allowed, given what a previous download proved about it?
///
/// Static and separate so the deny-list can be driven in tests without a download
/// service. The failure it guards against is invisible from outside: a filter that denies
/// everything leaves every track unfetchable and looks exactly like Soulseek having no
/// copies.
pub fn candidate_allowed(
    hit: &SoulseekFileHit,
    deny_list: Option<&RejectedPeerRegistry>,
    enabled: bool,
) -> bool {
    match deny_list {
        Some(deny_list) if enabled => !deny_list.is_denied(Some(&hit.username), Some(&hit.filename)),
        _ => true,
    }
}

/// slskd marks a transfer Succeeded before moving the file out of its incomplete
/// directory, and on bind mounts that move is a copy that can take seconds for a FLAC.
/// Without this window the attempt fails on "no file on disk" and the next peer
/// re-downloads the same track. The incomplete folder is never searched (#69), so a full-size
/// copy that slskd has not moved yet cannot end the wait early. A cancelled caller gets one
/// final check instead of a wait.
pub async fn retry_resolve(
    mut resolve: impl FnMut() -> Option<String>,
    max_wait: Duration,
    poll_interval: Duration,
    ct: &CancellationToken,
) -> Option<String> {
    let deadline = Instant::now() + max_wait;
    loop {
        let path = resolve();
        if path.is_some() || Instant::now() >= deadline {
            return path;
        }
        tokio::select! {
            biased;
            _ = ct.cancelled() => {
                // Caller left: no point waiting out the window, but the file may
                // have just landed, so look once more before giving up.
                return resolve();
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }
}

/// A new job folder, relative to slskd's downloads directory. A dot folder, so Navidrome's scan
/// never sees a download before Octo has placed it.
pub fn new_job_dir() -> String {
    format!("{INCOMING_FOLDER_NAME}/slskd/{}", new_guid())
}

/// What to do with a likely transcode, given the one already held back, if any. The first is
/// held. A later one replaces it only with a higher cutoff, which is the higher bitrate it
/// was made from. And the resolver matches on leaf name and size, so a later peer offering
/// the same rip can resolve to the very file already held back: that is not a new file, and
/// deleting it as one would lose the only copy.
pub fn weigh_transcode(
    held_path: Option<&str>,
    held_cutoff_hz: Option<f64>,
    path: &str,
    cutoff_hz: Option<f64>,
) -> ReserveChoice {
    match held_path {
        Some(held) if same_path(held, path) => ReserveChoice::AlreadyHeld,
        None => ReserveChoice::Hold,
        Some(_) if cutoff_hz.unwrap_or(0.0) > held_cutoff_hz.unwrap_or(0.0) => ReserveChoice::Hold,
        Some(_) => ReserveChoice::DiscardNew,
    }
}

fn same_path(a: &str, b: &str) -> bool {
    get_full_path(a) == get_full_path(b)
}

/// The folder names a finished download is never taken from. The default is always in the
/// list, so the usual layout stays safe even when slskd's options cannot be read.
pub fn excluded_folder_names(slskd_incomplete_dir: Option<&str>) -> Vec<String> {
    let normalized = slskd_incomplete_dir.unwrap_or("").replace('\\', "/");
    let last = normalized.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    // Octo's own staging, slskd job folders included: a file there belongs to one download,
    // and a search by name must never hand it to another.
    if dotnet::is_blank(last) || dotnet::eq_ignore_case(last, DEFAULT_INCOMPLETE_FOLDER_NAME) {
        vec![DEFAULT_INCOMPLETE_FOLDER_NAME.into(), INCOMING_FOLDER_NAME.into()]
    } else {
        vec![
            DEFAULT_INCOMPLETE_FOLDER_NAME.into(),
            last.to_string(),
            INCOMING_FOLDER_NAME.into(),
        ]
    }
}

/// The file an attempt asked slskd to put in its own folder. In that folder only: the leaf
/// itself, slskd's renamed copy of it, or, since slskd may clean a name up, the one file of
/// exactly this size, then the one within the usual size drift. Never anything outside it.
pub fn resolve_in_job(
    roots: &[String],
    job_dir: &str,
    remote_filename: &str,
    expected_size: i64,
    require_exact_size: bool,
) -> Option<String> {
    let normalized = remote_filename.replace('\\', "/");
    let leaf = normalized.split('/').rfind(|s| !s.is_empty())?;
    let renamed = renamed_pattern(leaf);

    for root in roots {
        let folder = combine(root, job_dir);
        let look = || -> std::io::Result<Option<String>> {
            if !Path::new(&folder).is_dir() {
                return Ok(None);
            }
            let exact = combine(&folder, leaf);
            if file_matches(&exact, expected_size, require_exact_size) {
                return Ok(Some(exact));
            }
            let files = files_in(&folder)?;
            let mut renamed_copies: Vec<&String> = files
                .iter()
                .filter(|f| {
                    renamed.is_match(file_name(f)) && file_matches(f, expected_size, require_exact_size)
                })
                .collect();
            renamed_copies.sort_by_key(|f| std::cmp::Reverse(creation_time(f)));
            if let Some(pick) = renamed_copies.first() {
                return Ok(Some((*pick).clone()));
            }
            let same_size: Vec<&String> = files
                .iter()
                .filter(|f| file_length(f) == Some(expected_size))
                .collect();
            if same_size.len() == 1 {
                return Ok(Some(same_size[0].clone()));
            }
            if !require_exact_size {
                let near: Vec<&String> = files
                    .iter()
                    .filter(|f| file_matches(f, expected_size, false))
                    .collect();
                if near.len() == 1 {
                    return Ok(Some(near[0].clone()));
                }
            }
            Ok(None)
        };
        // A folder that cannot be read holds nothing this attempt can use.
        if let Ok(Some(found)) = look() {
            return Some(found);
        }
    }
    None
}

/// `require_exact_size`: drop the usual near-miss tolerance. Used when a transfer was interrupted,
/// where a slightly-short file is more likely truncated than size drift.
///
/// `excluded_folders`: slskd's incomplete folder names. A full-size copy there is one slskd is
/// about to move and delete; taking it is how a song was tagged at a path that was gone a second
/// later (#69).
pub fn resolve_local_path(
    remote_filename: &str,
    expected_size: i64,
    require_exact_size: bool,
    roots: &[String],
    excluded_folders: &[String],
    on_scan_error: Option<&dyn Fn(&str, &str)>,
) -> Option<String> {
    let normalized = remote_filename.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    let leaf = *segments.last()?;
    let parent = (segments.len() >= 2).then(|| segments[segments.len() - 2]);

    let usable = |root: &str, path: &str| {
        !in_excluded_folder(root, path, parent, excluded_folders)
            && file_matches(path, expected_size, require_exact_size)
    };

    for root in roots {
        if let Some(parent) = parent {
            let candidate = combine(&combine(root, parent), leaf);
            if usable(root, &candidate) {
                return Some(candidate);
            }
        }
        let flat = combine(root, leaf);
        if usable(root, &flat) {
            return Some(flat);
        }
    }

    // slskd never overwrites: when the name is taken it saves the new file as
    // <name>_<DateTime.UtcNow ticks><ext> in the same folder. Ticks are 18 digits today, so
    // demanding 15 keeps a peer's own "Song_2.flac" out. Only a fallback, behind any exact name.
    let (stem, ext) = split_extension(leaf);
    let renamed = renamed_pattern(leaf);

    for root in roots {
        if !Path::new(root).is_dir() {
            continue;
        }
        // `Directory.EnumerateFiles(root, stem + "*" + ext, AllDirectories)`: the name filters
        // below are stricter than the wildcard, so every file under the root is looked at.
        let found = files_under(root).map(|files| {
            files
                .into_iter()
                .filter(|p| {
                    let name = file_name(p);
                    name.starts_with(stem) && name.ends_with(ext) && usable(root, p)
                })
                .collect::<Vec<String>>()
        });
        match found {
            Ok(found) => {
                let pick = newest(found.iter().filter(|p| file_name(p) == leaf)).or_else(|| {
                    newest(found.iter().filter(|p| {
                        parent.is_some()
                            && renamed.is_match(file_name(p))
                            && directory_name(p).as_deref().map(file_name) == parent
                    }))
                });
                if pick.is_some() {
                    return pick;
                }
            }
            Err(e) => {
                if let Some(report) = on_scan_error {
                    report(root, &e.to_string());
                }
            }
        }
    }

    None
}

/// Whether a path sits in one of slskd's incomplete folders below the root. The file's own
/// folder is exempt when it is the peer's folder name, which slskd keeps when it files a
/// finished download: a peer who named a folder "incomplete" is not slskd's work area.
pub fn in_excluded_folder(
    root: &str,
    path: &str,
    remote_parent: Option<&str>,
    excluded_folders: &[String],
) -> bool {
    if excluded_folders.is_empty() {
        return false;
    }
    let relative = relative_path(root, path);
    let parts: Vec<&str> = relative.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    for i in 0..parts.len().saturating_sub(1) {
        if !excluded_folders
            .iter()
            .any(|e| dotnet::eq_ignore_case(e, parts[i]))
        {
            continue;
        }
        if i == parts.len() - 2 && remote_parent.is_some_and(|p| dotnet::eq_ignore_case(parts[i], p)) {
            continue;
        }
        return true;
    }
    false
}

fn file_matches(path: &str, expected_size: i64, require_exact_size: bool) -> bool {
    match file_length(path) {
        None => false,
        Some(actual) if actual == expected_size => true,
        Some(actual) => !require_exact_size && (actual - expected_size).abs() < 64 * 1024,
    }
}

/// Read the real duration off the downloaded file and compare it against what the
/// catalog says the track should be. Anything unknown or unreadable passes: this
/// exists to catch a confidently wrong file, not to reject an unusual one.
fn downloaded_duration_matches(path: &str, expected_seconds: Option<i32>) -> (bool, i32) {
    let Some(expected) = expected_seconds.filter(|&e| e > 0) else {
        return (true, 0);
    };

    let actual_seconds = match TagFile::open(path) {
        Ok(file) => file.duration_seconds(),
        Err(e) => {
            debug!("Could not read duration of {path}: {e}");
            return (true, 0);
        }
    };

    if actual_seconds <= 0 {
        return (true, actual_seconds);
    }
    (
        (actual_seconds - expected).abs()
            <= octo_core::soulseek::soulseek_download_service::DURATION_TOLERANCE_SECONDS,
        actual_seconds,
    )
}

/// Remove a file we have judged to be the wrong recording, so it does not sit in the
/// music folder waiting to be scanned or matched by a later resolve.
///
/// Only deletes what this attempt actually created. The resolver matches on leaf
/// name and approximate size across the whole library, so without that guard a bad
/// match could delete a file the user already owned.
fn discard_rejected_download(path: &str, attempt_started_utc: DateTime<Utc>) {
    if !file_exists(path) {
        return;
    }
    let created: DateTime<Utc> = creation_time(path).into();
    if created < attempt_started_utc - TimeDelta::seconds(5) {
        warn!("Leaving {path} in place: it predates this download, so it is not ours to delete");
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => info!("Deleted rejected download {path}"),
        Err(e) => warn!("Could not delete mismatched download {path}: {e}"),
    }
}

/// Clear what a crash or a failed download left in the staging folder. A day old is long past
/// any download still in flight, and nothing outside this one folder is touched.
fn sweep_incoming(incoming: &str) {
    let sweep = || -> std::io::Result<()> {
        if !Path::new(incoming).is_dir() {
            return Ok(());
        }
        let root = format!("{}/", get_full_path(incoming).trim_end_matches('/'));
        for file in files_in(incoming)? {
            if !get_full_path(&file).starts_with(&root) {
                continue;
            }
            if age_of(&std::fs::metadata(&file)?.modified()?) < Duration::from_secs(24 * 3600) {
                continue;
            }
            std::fs::remove_file(&file)?;
            info!("Removed a stale staged download {file}");
        }
        Ok(())
    };
    if let Err(e) = sweep() {
        debug!("Could not sweep {incoming}: {e}");
    }
}

// ---- small helpers -------------------------------------------------------------------------

/// The routing's artist and title, for messages.
fn names(routing: &SoulseekRouting) -> (String, String) {
    (
        routing.artist.clone().unwrap_or_default(),
        routing.title.clone().unwrap_or_default(),
    )
}

/// A nullable value as .NET formats it in a message: empty when null.
fn display_option(value: Option<i32>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// `{0:P0}` for the log: a whole percentage.
fn percent(fraction: f64) -> String {
    format!("{}%", dotnet::round(fraction * 100.0, 0))
}

/// `Guid.NewGuid().ToString("N")`.
fn new_guid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// `Path.Combine(a, b)`.
fn combine(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `Path.GetFileName`.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `Path.GetDirectoryName`.
fn directory_name(path: &str) -> Option<String> {
    let slash = path.rfind('/')?;
    Some(if slash == 0 {
        "/".to_string()
    } else {
        path[..slash].to_string()
    })
}

/// `Path.GetFileNameWithoutExtension` and `Path.GetExtension` of a file name.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => (&name[..dot], &name[dot..]),
        Some(dot) => (&name[..dot], ""),
        None => (name, ""),
    }
}

/// slskd's name for a newcomer whose name was taken: `<stem>_<15+ digits><ext>`.
fn renamed_pattern(leaf: &str) -> Regex {
    let (stem, ext) = split_extension(leaf);
    Regex::new(&format!(
        "^{}_[0-9]{{15,}}{}$",
        regex::escape(stem),
        regex::escape(ext)
    ))
    .expect("an escaped pattern compiles")
}

fn file_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_file()
}

fn file_length(path: &str) -> Option<i64> {
    let metadata = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    i64::try_from(metadata.len()).ok()
}

/// `File.GetCreationTimeUtc`: the birth time where the file system keeps one, the last write
/// otherwise.
fn creation_time(path: &str) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.created().or_else(|_| m.modified()))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn age_of(time: &SystemTime) -> Duration {
    SystemTime::now().duration_since(*time).unwrap_or(Duration::ZERO)
}

fn newest<'a>(paths: impl Iterator<Item = &'a String>) -> Option<String> {
    let mut paths: Vec<&String> = paths.collect();
    paths.sort_by_key(|p| std::cmp::Reverse(creation_time(p)));
    paths.first().map(|p| (*p).clone())
}

/// `Path.GetRelativePath(root, path)` for a path under the root; the path itself otherwise.
fn relative_path(root: &str, path: &str) -> String {
    let root = get_full_path(root);
    let path = get_full_path(path);
    let prefix = format!("{}/", root.trim_end_matches('/'));
    path.strip_prefix(&prefix).map(str::to_string).unwrap_or(path)
}

/// The files directly in a folder (`Directory.EnumerateFiles(folder)`).
fn files_in(folder: &str) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let path = entry?.path();
        if path.is_file() {
            files.push(path.to_string_lossy().into_owned());
        }
    }
    Ok(files)
}

/// Every file under a folder, any depth. A folder that cannot be read fails the whole walk, as
/// `Directory.EnumerateFiles(.., AllDirectories)` threw.
fn files_under(root: &str) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    let mut folders = VecDeque::from([root.to_string()]);
    while let Some(folder) = folders.pop_front() {
        for entry in std::fs::read_dir(&folder)? {
            let path = entry?.path();
            if path.is_dir() {
                folders.push_back(path.to_string_lossy().into_owned());
            } else if path.is_file() {
                files.push(path.to_string_lossy().into_owned());
            }
        }
    }
    Ok(files)
}

/// The first folder named `name` under `root`, any depth, that `accept` takes
/// (`Directory.EnumerateDirectories(root, name, AllDirectories).FirstOrDefault(..)`).
fn find_directory(root: &str, name: &str, accept: impl Fn(&str) -> bool) -> std::io::Result<Option<String>> {
    let mut folders = VecDeque::from([root.to_string()]);
    while let Some(folder) = folders.pop_front() {
        for entry in std::fs::read_dir(&folder)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let text = path.to_string_lossy().into_owned();
            if file_name(&text) == name && accept(&text) {
                return Ok(Some(text));
            }
            folders.push_back(text);
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "soulseek_download_service_tests.rs"]
mod tests;
