//! STUB(4-E): replaced when 4-E lands with the port of
//! `Services/Lidarr/LidarrHeartAcquisitionService.cs`. Only the interface is here.

use async_trait::async_trait;

/// Hearts through Lidarr.
#[async_trait]
pub trait ILidarrHeartAcquisitionService: Send + Sync {
    /// True once the song is in the library: Lidarr brought it and it passed Octo's checks, or it
    /// was there already. False when Lidarr could not get it, so the heart's next source tries.
    /// Waits for Lidarr's import, up to Lidarr's import timeout. (`notify_failure` was `true`
    /// by default, `requested_by` null.) `Err` where the C# threw.
    async fn try_acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
    ) -> anyhow::Result<bool>;

    /// True once every song of the album is in the library; false leaves the rest to
    /// the next source, which skips the songs that did land.
    async fn try_acquire_album(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
    ) -> anyhow::Result<bool>;
}
