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
//! It knows nothing and fetches nothing: a heart that reaches it fails with a reason that says
//! so.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::DownloadSource;
use tokio_util::sync::CancellationToken;

use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::ReplacementHandoff;

fn not_ported(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} is not available in this build yet")
}

/// STUB(4-C): replaced by `SoulseekDownloadService`.
pub struct NotPortedDownloadService;

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

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}
