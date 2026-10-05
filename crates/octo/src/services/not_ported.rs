//! A stand-in for the service a later task ports, so the acquisition pipeline (4-D) can be wired
//! into the app state now. Not a C# file. It is replaced in `app.rs` by the real service when
//! its task lands, and this file goes with it:
//!
//! - STUB(4-C): [`NotPortedDownloadService`] for `SoulseekDownloadService`. The real one is a
//!   `DownloadBackend` under `services::common::BaseDownloadService` (4-B), built with
//!   `BaseDownloadService::new(DownloadCore, DownloadServices, backend)`; the tagging services
//!   it takes are `AppInner`'s `release_identifier`, `loudness_meter` and
//!   `download_cover_resolver`.
//!
//! It fetches nothing: a heart that reaches it fails with a reason that says so. It does play
//! an outside song's YouTube preview (`SoulseekDownloadService.GetDirectStreamAsync`, ported
//! early by 6-A2 so `stream` works for outside songs): the registry or the id itself names the
//! song, the shim finds and streams it.

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::DownloadSource;
use tokio_util::sync::CancellationToken;

use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::ReplacementHandoff;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::you_tube::YouTubeResolver;

fn not_ported(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} is not available in this build yet")
}

/// STUB(4-C): replaced by `SoulseekDownloadService`.
#[derive(Default)]
pub struct NotPortedDownloadService {
    /// What the preview needs; without it no preview plays.
    preview: Option<(Arc<ExternalIdRegistry>, Arc<YouTubeResolver>)>,
}

impl NotPortedDownloadService {
    /// The stand-in with the preview path, over the id registry and the shim.
    pub fn with_preview(registry: Arc<ExternalIdRegistry>, youtube: Arc<YouTubeResolver>) -> Self {
        NotPortedDownloadService {
            preview: Some((registry, youtube)),
        }
    }
}

#[async_trait]
impl IDownloadService for NotPortedDownloadService {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        Err(not_ported("Downloading"))
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        Err(not_ported("Downloading"))
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        Err(not_ported("Downloading"))
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        Err(not_ported("Downloading"))
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        false
    }

    /// Streaming path (every play of an unowned radio track): an instant lossy preview via
    /// YouTube (yt-dlp).
    async fn get_direct_stream(
        &self,
        external_provider: &str,
        external_id: &str,
        range_header: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        let Some((registry, youtube)) = &self.preview else {
            return Ok(None);
        };
        if !external_provider.eq_ignore_ascii_case(SoulseekMetadataService::PROVIDER_NAME) {
            return Ok(None);
        }

        let shared = registry.lookup(external_id);
        let Some(routing) = shared
            .as_ref()
            .map(|r| r.snapshot())
            .or_else(|| SoulseekMetadataService::try_decode_external_id(Some(external_id)))
        else {
            return Ok(None);
        };

        let mut video_id = routing.you_tube_id.clone().filter(|v| !v.is_empty());
        if video_id.is_none() && routing.has_artist_title() {
            let query = format!(
                "{} {}",
                routing.artist.as_deref().unwrap_or(""),
                routing.title.as_deref().unwrap_or("")
            );
            let hit = youtube.search(&query, routing.duration, false).await;
            video_id = hit.map(|h| h.video_id).filter(|v| !v.is_empty());
            // Cache back on the routing so a second click on the same placeholder
            // skips the yt-dlp ytsearch1: round trip — that 3-8s saving is the
            // difference between Arpeggi (~10s HTTP timeout) playing the song or
            // canceling and falling back to a local one. The routing object is
            // shared via the registry singleton, so this mutation is visible to
            // every subsequent stream request for this id.
            if let (Some(video_id), Some(shared)) = (&video_id, &shared) {
                shared.lock().you_tube_id = Some(video_id.clone());
            }
        }
        let Some(video_id) = video_id else {
            return Ok(None);
        };

        let Some(opened) = youtube.open_stream(&video_id, range_header).await else {
            tracing::warn!("yt-dlp shim failed to open stream for vid={video_id}");
            return Ok(None);
        };

        tracing::info!(
            "YouTube preview '{} - {}' (vid={video_id}, status={}, {} bytes{})",
            routing.artist.as_deref().unwrap_or(""),
            routing.title.as_deref().unwrap_or(""),
            opened.status_code,
            opened.content_length.map(|l| l.to_string()).unwrap_or_default(),
            opened
                .content_range
                .as_deref()
                .map(|r| format!(", range={r}"))
                .unwrap_or_default()
        );

        Ok(Some(DirectStreamInfo {
            audio_stream: Box::pin(opened.response.bytes_stream().map_err(std::io::Error::other)),
            content_type: opened.content_type,
            content_length: opened.content_length,
            quality: Some("youtube-m4a".to_string()),
            status_code: opened.status_code,
            content_range: opened.content_range,
        }))
    }
}
