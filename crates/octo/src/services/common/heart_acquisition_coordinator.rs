//! Port of `Services/Common/HeartAcquisitionCoordinator.cs`.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::settings::{DownloadSource, HeartDownloadSource, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::TrackAcquisitionQueue;
use super::{
    AcquisitionState, AcquisitionTracker, HeldAcquisition, HeldKind, SoulseekHoldStore, StarOnArrival,
};
use crate::services::i_download_service::IDownloadService;
use crate::services::library::{HeartOwnership, OwnedCopy};
use crate::services::lidarr::ILidarrHeartAcquisitionService;
use crate::services::soulseek::soulseek_link::SoulseekLinkState;
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink};

/// The coordinator's optional collaborators, each `null` by default in the C# constructor.
#[derive(Default, Clone)]
pub struct HeartCoordinatorExtras {
    /// The chain is the only place that knows which failure is the last, so it is the one that
    /// reports it. Without one, nothing is reported.
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub id_registry: Option<Arc<ExternalIdRegistry>>,
    /// Without one nothing waits for Soulseek, which is how it always was.
    pub soulseek: Option<Arc<dyn ISoulseekLink>>,
    pub holds: Option<Arc<SoulseekHoldStore>>,
    /// Without it every heart goes down the chain, and the download itself notices a song that
    /// is already there, as before.
    pub owned: Option<Arc<HeartOwnership>>,
    pub stars: Option<Arc<StarOnArrival>>,
}

/// Routes explicit heart gestures, and plays when DownloadOnPlay or LidarrAlbumOnPlay ask
/// for it.
pub struct HeartAcquisitionCoordinator {
    /// `IOptionsMonitor<SubsonicSettings>`: the chain is read at every heart.
    settings: Arc<SettingsStore>,
    direct_queue: Arc<TrackAcquisitionQueue>,
    direct_downloads: Arc<dyn IDownloadService>,
    lidarr: Arc<dyn ILidarrHeartAcquisitionService>,
    extras: HeartCoordinatorExtras,

    // Plays waiting for Soulseek, one per song however often it is played meanwhile.
    held_plays: Mutex<HashSet<String>>,

    // Track ids handed to Lidarr on play, so a replay does not search every indexer again.
    // Cleared when full rather than aged: forgetting costs at most one more search.
    lidarr_plays: Mutex<HashSet<String>>,
}

impl HeartAcquisitionCoordinator {
    const LIDARR_PLAY_MEMORY: usize = 10_000;

    pub fn new(
        settings: Arc<SettingsStore>,
        direct_queue: Arc<TrackAcquisitionQueue>,
        direct_downloads: Arc<dyn IDownloadService>,
        lidarr: Arc<dyn ILidarrHeartAcquisitionService>,
        extras: HeartCoordinatorExtras,
    ) -> Arc<Self> {
        Arc::new(HeartAcquisitionCoordinator {
            settings,
            direct_queue,
            direct_downloads,
            lidarr,
            extras,
            held_plays: Mutex::new(HashSet::new()),
            lidarr_plays: Mutex::new(HashSet::new()),
        })
    }

    fn tracker(&self) -> Option<&Arc<AcquisitionTracker>> {
        self.extras.tracker.as_ref()
    }

    pub fn queue_track(self: &Arc<Self>, provider: &str, external_id: &str, requested_by: Option<&str>) {
        let coordinator = Arc::clone(self);
        let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
        // Fire and forget: a chain that stops on an error is left as it stands, as the C#
        // discarded the task.
        tokio::spawn(async move {
            let _ = coordinator
                .acquire_track(&provider, &external_id, requested_by.as_deref(), None)
                .await;
        });
    }

    /// A client started an external track from its first byte.
    pub fn queue_play(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
    ) {
        let settings = self.settings.current().subsonic.clone();
        // WaitForLosslessOnPlay acquires the track itself, from its own source.
        if settings.download_on_play
            && !settings.wait_for_lossless_on_play
            && let Some(source) = self.play_source()
        {
            let holds = self
                .extras
                .soulseek
                .as_ref()
                .is_some_and(|link| link.hold_limit() > TimeDelta::zero());
            if source == DownloadSource::YouTube || !holds {
                self.queue_play_download(provider, external_id, source, requested_by, client_id, owner);
            } else {
                let coordinator = Arc::clone(self);
                let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
                let (client_id, owner) = (client_id.map(str::to_string), owner.map(str::to_string));
                tokio::spawn(async move {
                    coordinator
                        .hold_then_queue_play(
                            &provider,
                            &external_id,
                            source,
                            requested_by.as_deref(),
                            client_id.as_deref(),
                            owner.as_deref(),
                        )
                        .await
                });
            }
        }
        if !settings.lidarr_album_on_play {
            return;
        }
        let key = format!("{provider}:{external_id}");
        let added = {
            let mut plays = self.lidarr_plays.lock();
            if plays.len() >= Self::LIDARR_PLAY_MEMORY {
                plays.clear();
            }
            plays.insert(key.clone())
        };
        if added {
            let coordinator = Arc::clone(self);
            let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
            tokio::spawn(async move {
                coordinator
                    .hand_play_to_lidarr(&key, &provider, &external_id, requested_by.as_deref())
                    .await
            });
        }
    }

    fn queue_play_download(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
    ) {
        let on_queued = || {
            // Same row a heart gets, opened before the worker can report its first stage.
            let routing = self
                .extras
                .id_registry
                .as_ref()
                .and_then(|r| r.lookup(external_id))
                .map(|shared| shared.snapshot());
            if let Some(tracker) = self.tracker() {
                tracker.begin(
                    provider,
                    external_id,
                    client_id,
                    owner,
                    routing.as_ref().and_then(|r| r.artist.as_deref()),
                    routing.as_ref().and_then(|r| r.title.as_deref()),
                    routing.as_ref().and_then(|r| r.album.as_deref()),
                );
                tracker.stage(
                    provider,
                    external_id,
                    AcquisitionState::Queued,
                    Some(if source == DownloadSource::YouTube {
                        "YouTube"
                    } else {
                        "Soulseek"
                    }),
                    None,
                );
            }
        };
        let Some(request) =
            self.direct_queue
                .try_enqueue_play(provider, external_id, source, requested_by, Some(&on_queued))
        else {
            return;
        };
        let tracker = self.tracker().cloned();
        let (provider, external_id) = (provider.to_string(), external_id.to_string());
        tokio::spawn(async move {
            let Err(reason) = request.completion.wait().await else {
                return;
            };
            debug!("Play download failed for {provider}:{external_id}: {reason}");
            // A heart that joined owns the row, and may still be trying its next source.
            if !request.is_star
                && !request.heart_joined()
                && let Some(tracker) = tracker
            {
                tracker.fail(&provider, &external_id, Some(&reason.to_string()));
            }
        });
    }

    /// A play during a Soulseek outage waits for it like a heart, but only in memory: a play is a
    /// hint, and one lost to a restart costs a replay.
    async fn hold_then_queue_play(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
    ) {
        let key = format!("{provider}:{external_id}");
        let Some(link) = self.extras.soulseek.clone() else {
            return;
        };
        if link.read(false).await.map(|r| r.link) == Some(SoulseekLinkState::NotLoggedIn) {
            if !self.held_plays.lock().insert(key.clone()) {
                return;
            }
            link.wait_for_login(link.utc_now() + link.hold_limit()).await;
            self.held_plays.lock().remove(&key);
        }
        self.queue_play_download(provider, external_id, source, requested_by, client_id, owner);
    }

    async fn hand_play_to_lidarr(
        &self,
        key: &str,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
    ) {
        match self
            .lidarr
            .try_acquire_track(provider, external_id, false, requested_by)
            .await
        {
            Ok(true) => return,
            Ok(false) => debug!("Lidarr took no album for played track {provider}:{external_id}"),
            Err(e) => debug!("Lidarr hand-off failed for played track {provider}:{external_id}: {e}"),
        }
        // Not handed off, so the next play of this track tries again.
        self.lidarr_plays.lock().remove(key);
    }

    pub fn queue_album(
        self: &Arc<Self>,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) {
        let coordinator = Arc::clone(self);
        let (provider, album_id, requested_by) = owned3(provider, album_external_id, requested_by);
        tokio::spawn(async move {
            let _ = coordinator
                .acquire_album(&provider, &album_id, requested_by.as_deref(), None)
                .await;
        });
    }

    /// A heart that was waiting for Soulseek when Octo restarted. Its saved start time
    /// keeps the original deadline; the entry goes once the chain has finished either way.
    pub(crate) fn resume_track(self: &Arc<Self>, held: HeldAcquisition) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let chain = coordinator
                .acquire_track(
                    &held.provider,
                    &held.external_id,
                    held.requested_by.as_deref(),
                    Some(held.held_since_utc),
                )
                .await;
            coordinator.resumed(&held, chain);
        });
    }

    pub(crate) fn resume_album(self: &Arc<Self>, held: HeldAcquisition) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let chain = coordinator
                .acquire_album(
                    &held.provider,
                    &held.external_id,
                    held.requested_by.as_deref(),
                    Some(held.held_since_utc),
                )
                .await;
            coordinator.resumed(&held, chain);
        });
    }

    /// `ResumeAsync`'s end: a chain that failed is logged, and the entry goes either way.
    fn resumed(&self, held: &HeldAcquisition, chain: anyhow::Result<()>) {
        if let Err(e) = chain {
            warn!("Resumed {} failed: {e}", held.key());
        }
        if let Some(holds) = &self.extras.holds {
            holds.release(&held.key());
        }
    }

    /// The song chain. `Err` where the C# let an exception out: a Lidarr step that failed.
    pub(crate) async fn acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        if self.already_yours(provider, external_id, requested_by).await {
            return Ok(());
        }
        let steps = self.enabled_steps(false);
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            // One entry for the whole chain. The first direct source is still waiting for the
            // worker; every later source, and Lidarr, starts over by looking.
            if let Some(tracker) = self.tracker() {
                let note = (index > 0).then(|| {
                    format!(
                        "{} couldn't get it, trying {}",
                        source_name(steps[index - 1]),
                        source_name(step)
                    )
                });
                tracker.stage(
                    provider,
                    external_id,
                    if index == 0 && step != HeartDownloadSource::Lidarr {
                        AcquisitionState::Queued
                    } else {
                        AcquisitionState::Searching
                    },
                    Some(source_name(step)),
                    note.as_deref(),
                );
            }
            if step == HeartDownloadSource::Lidarr {
                // A Lidarr that fails ends the chain with its error, as its exception did.
                if self
                    .lidarr
                    .try_acquire_track(provider, external_id, is_last, requested_by)
                    .await?
                {
                    return Ok(());
                }
                continue;
            }

            // Soulseek waits out an outage, before the step and again when slskd lost its login
            // partway through it. Every other source, and Soulseek once the wait is over, goes on
            // exactly as before.
            let wait_until = self.soulseek_deadline(&steps, index, held_since_utc);
            let failure = loop {
                self.hold_for_soulseek(HeldKind::Track, provider, external_id, requested_by, wait_until)
                    .await;
                let completion = self.direct_queue.enqueue(
                    provider,
                    external_id,
                    true,
                    false,
                    true,
                    Some(to_direct_source(step)),
                    is_last,
                    // Passed on every step, not only the first. A track that fails its way
                    // down the source chain is still the same person's star.
                    requested_by,
                    false,
                    None,
                );
                match completion.wait().await {
                    Ok(_) => return Ok(()),
                    Err(failure) => {
                        if !self.soulseek_dropped(wait_until).await {
                            break failure;
                        }
                        info!(
                            "Soulseek lost its connection while getting {provider}:{external_id}; waiting for it rather than moving on"
                        );
                    }
                }
            };

            // A muted mid-chain failure must still leave a trace, or a track that
            // silently fell through every source is undiagnosable from the logs.
            warn!(
                "Heart source {} failed for track {provider}:{external_id}: {failure}",
                source_enum_name(step)
            );
            if is_last {
                if let Some(tracker) = self.tracker() {
                    tracker.fail(provider, external_id, Some(&failure.to_string()));
                }
                return Ok(());
            }
            // The next enabled source owns the fallback.
        }
        // Only reached when the last source was Lidarr and it said no. It records its own
        // reason first; this is the fallback when it could not.
        if let Some(tracker) = self.tracker() {
            tracker.fail(
                provider,
                external_id,
                Some("No download source could get this song."),
            );
        }
        Ok(())
    }

    /// The album chain. `Err` where the C# let an exception out: a Lidarr step that failed.
    pub(crate) async fn acquire_album(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        held_since_utc: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        if self
            .album_already_yours(provider, album_external_id, requested_by)
            .await
        {
            return Ok(());
        }
        let steps = self.enabled_steps(true);
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            if step == HeartDownloadSource::Lidarr {
                if self
                    .lidarr
                    .try_acquire_album(provider, album_external_id, is_last, requested_by)
                    .await?
                {
                    return Ok(());
                }
                continue;
            }

            let wait_until = self.soulseek_deadline(&steps, index, held_since_utc);
            let failure = loop {
                self.hold_for_soulseek(
                    HeldKind::Album,
                    provider,
                    album_external_id,
                    requested_by,
                    wait_until,
                )
                .await;
                let mut failure = None;
                match self
                    .direct_downloads
                    .download_album_with_source(
                        provider,
                        album_external_id,
                        to_direct_source(step),
                        !is_last,
                        &CancellationToken::new(),
                        requested_by.map(|r| vec![r.to_string()]),
                    )
                    .await
                {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(e) => failure = Some(e),
                }
                // A walk that came up short because slskd dropped partway: the tracks it got stay,
                // and the next walk skips them.
                if !self.soulseek_dropped(wait_until).await {
                    break failure;
                }
                info!(
                    "Soulseek lost its connection during album {provider}:{album_external_id}; waiting for it rather than moving on"
                );
            };

            if let Some(failure) = failure {
                warn!(
                    "Heart source {} failed for album {provider}:{album_external_id}: {failure}",
                    source_enum_name(step)
                );
                if is_last {
                    if let Some(tracker) = self.tracker() {
                        tracker.fail_album(provider, album_external_id, Some(&failure.to_string()));
                    }
                    return Ok(());
                }
            }
            // Continue down the configured priority list.
        }
        // Every source has had its go. Tracks the last walk already settled keep what it said;
        // this only closes the ones nothing finished.
        if let Some(tracker) = self.tracker() {
            tracker.fail_album(
                provider,
                album_external_id,
                Some("No download source could get this track."),
            );
        }
        Ok(())
    }

    fn enabled_steps(&self, album_heart: bool) -> Vec<HeartDownloadSource> {
        self.settings
            .current()
            .subsonic
            .effective_heart_download_sources()
            .into_iter()
            .filter(|step| {
                if album_heart {
                    step.album_enabled == Some(true)
                } else {
                    step.song_enabled == Some(true)
                }
            })
            .map(|step| step.source)
            .collect()
    }

    fn play_source(&self) -> Option<DownloadSource> {
        let sources: Vec<HeartDownloadSource> = self
            .enabled_steps(false)
            .into_iter()
            .filter(|s| *s != HeartDownloadSource::Lidarr)
            .collect();
        let first = *sources.first()?;
        Some(if first == HeartDownloadSource::YouTube {
            DownloadSource::YouTube
        } else if sources.contains(&HeartDownloadSource::YouTube) {
            DownloadSource::SoulseekThenYouTube
        } else {
            DownloadSource::Soulseek
        })
    }

    /// A heart on a song already in the library is a favorite, not a download. Asked first, so
    /// it never waits behind other downloads or a Soulseek outage, and never sends Lidarr for a
    /// whole album. The row closes on the library's own song, which favorites it for whoever
    /// hearted it (StarOnArrival); an owned lossy copy is also queued for Better quality.
    async fn already_yours(&self, provider: &str, external_id: &str, requested_by: Option<&str>) -> bool {
        let Some(owned) = &self.extras.owned else {
            return false;
        };
        let Some((song, copy)) = owned.find_song(provider, external_id).await else {
            return false;
        };
        let upgrading = owned.queue_upgrade_if_wanted(&copy, &song, requested_by);
        // Before the row closes, so the close never reads as a download that landed.
        if let Some(stars) = &self.extras.stars {
            stars.favorite_owned(
                provider,
                external_id,
                copy.navidrome_id.as_deref(),
                &song.artist,
                &song.title,
                &copy.absolute_path,
            );
        }
        info!(
            "Hearted '{} - {}' is already in the library ({}); favoriting it instead of downloading{}",
            song.artist,
            song.title,
            copy.suffix,
            if upgrading {
                ", and looking for a higher quality copy"
            } else {
                ""
            }
        );
        self.settle(provider, external_id, &copy);
        true
    }

    /// An album heart where every song is already in the library: each is favorited
    /// through its row, and so the album too. One missing song and the album goes down the chain,
    /// where the songs already there are skipped as before.
    async fn album_already_yours(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) -> bool {
        let Some(owned) = &self.extras.owned else {
            return false;
        };
        let Some((album, songs)) = owned.find_whole_album(provider, album_external_id).await else {
            return false;
        };
        let tracked: Vec<&(octo_core::models::domain::Song, OwnedCopy)> = songs
            .iter()
            .filter(|(song, _)| song.external_id.as_deref().is_some_and(|id| !id.is_empty()))
            .collect();
        if let Some(any_song) = songs.iter().find_map(|(_, copy)| copy.navidrome_id.as_deref())
            && let Some(stars) = &self.extras.stars
        {
            stars.favorite_owned_album(provider, album_external_id, any_song);
        }
        if let Some(tracker) = self.tracker() {
            let listed: Vec<(String, Option<String>, Option<String>, Option<String>)> = tracked
                .iter()
                .map(|(song, _)| {
                    (
                        song.external_id.clone().unwrap_or_default(),
                        Some(song.artist.clone()),
                        Some(song.title.clone()),
                        Some(album.title.clone()),
                    )
                })
                .collect();
            tracker.announce(provider, Some(album_external_id), None, &listed);
        }
        let mut upgrading = 0;
        for (song, copy) in &tracked {
            if owned.queue_upgrade_if_wanted(copy, song, requested_by) {
                upgrading += 1;
            }
            self.settle(
                song.external_provider.as_deref().unwrap_or(provider),
                song.external_id.as_deref().unwrap_or_default(),
                copy,
            );
        }
        info!(
            "Hearted album '{} - {}' is already in the library ({} songs); favoriting it instead of downloading{}",
            album.artist,
            album.title,
            songs.len(),
            if upgrading > 0 {
                format!(", {upgrading} queued for a higher quality copy")
            } else {
                String::new()
            }
        );
        true
    }

    /// Close a row on the library's copy: by its Navidrome id when known, which is
    /// immediate, or by its path, which the tracker watches until Navidrome shows it.
    fn settle(&self, provider: &str, external_id: &str, copy: &OwnedCopy) {
        let Some(tracker) = self.tracker() else { return };
        match &copy.navidrome_id {
            Some(library_id) => tracker.complete(provider, external_id, Some(library_id)),
            None => tracker.imported(provider, external_id, None, None, Some(&copy.absolute_path)),
        }
    }

    /// When a Soulseek step stops waiting for slskd, or `None` when it never waits: another
    /// source, no link, the wait switched off, or Lidarr later in the chain. The wait exists so
    /// an outage does not turn a lossless heart into a YouTube MP3; Lidarr can bring a lossless
    /// copy now.
    fn soulseek_deadline(
        &self,
        steps: &[HeartDownloadSource],
        index: usize,
        held_since_utc: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let link = self.extras.soulseek.as_ref()?;
        (steps[index] == HeartDownloadSource::Soulseek
            && link.hold_limit() > TimeDelta::zero()
            && !steps[index + 1..].contains(&HeartDownloadSource::Lidarr))
        .then(|| held_since_utc.unwrap_or_else(|| link.utc_now()) + link.hold_limit())
    }

    /// Waits while slskd says it is not logged in, up to `wait_until`. On disk for the length
    /// of the wait, so a restart picks the heart up again.
    async fn hold_for_soulseek(
        &self,
        kind: HeldKind,
        provider: &str,
        id: &str,
        requested_by: Option<&str>,
        wait_until: Option<DateTime<Utc>>,
    ) {
        let (Some(deadline), Some(link)) = (wait_until, self.extras.soulseek.as_ref()) else {
            return;
        };
        let Some(reading) = link
            .read(false)
            .await
            .filter(|r| r.link == SoulseekLinkState::NotLoggedIn)
        else {
            return;
        };

        let held = self.extras.holds.as_ref().map(|holds| {
            holds.hold(HeldAcquisition {
                kind,
                provider: provider.to_string(),
                external_id: id.to_string(),
                requested_by: requested_by.map(str::to_string),
                held_since_utc: deadline - link.hold_limit(),
            })
        });
        if kind == HeldKind::Track
            && let Some(tracker) = self.tracker()
        {
            tracker.stage(
                provider,
                id,
                AcquisitionState::Queued,
                Some("Soulseek"),
                Some(&format!(
                    "Waiting for Soulseek to come back, until {} UTC",
                    deadline.format("%H:%M")
                )),
            );
        }
        info!(
            "Soulseek is not connected (slskd says {}); holding {} {provider}:{id} until {} UTC",
            reading.state.as_deref().unwrap_or("not logged in"),
            kind.name(),
            deadline.format("%H:%M")
        );
        let back = link.wait_for_login(deadline).await;
        if back {
            info!("Soulseek is back; going on with {} {provider}:{id}", kind.name());
        } else {
            warn!(
                "Soulseek still not connected after the wait; {} {provider}:{id} goes on to the next source",
                kind.name()
            );
        }
        if kind == HeldKind::Track
            && let Some(tracker) = self.tracker()
        {
            tracker.stage(
                provider,
                id,
                AcquisitionState::Queued,
                Some("Soulseek"),
                Some(if back {
                    "Soulseek is back"
                } else {
                    "Soulseek did not come back in time"
                }),
            );
        }
        if let (Some(held), Some(holds)) = (held, &self.extras.holds) {
            holds.release(&held.key());
        }
    }

    /// True when the step just failed because slskd lost its login and there is still time
    /// to wait. A fresh read: the cached one may be from before the drop.
    async fn soulseek_dropped(&self, wait_until: Option<DateTime<Utc>>) -> bool {
        let (Some(deadline), Some(link)) = (wait_until, self.extras.soulseek.as_ref()) else {
            return false;
        };
        link.utc_now() < deadline
            && link.read(true).await.map(|r| r.link) == Some(SoulseekLinkState::NotLoggedIn)
    }
}

fn owned3(a: &str, b: &str, c: Option<&str>) -> (String, String, Option<String>) {
    (a.to_string(), b.to_string(), c.map(str::to_string))
}

fn source_name(source: HeartDownloadSource) -> &'static str {
    match source {
        HeartDownloadSource::Lidarr => "Lidarr",
        HeartDownloadSource::YouTube => "YouTube",
        _ => "Soulseek",
    }
}

/// The enum member's name, as `{Source}` interpolated it.
fn source_enum_name(source: HeartDownloadSource) -> &'static str {
    source_name(source)
}

fn to_direct_source(source: HeartDownloadSource) -> DownloadSource {
    match source {
        HeartDownloadSource::YouTube => DownloadSource::YouTube,
        _ => DownloadSource::Soulseek,
    }
}

#[cfg(test)]
#[path = "heart_acquisition_coordinator_tests.rs"]
mod tests;
