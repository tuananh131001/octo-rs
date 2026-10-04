//! STUB(5-A): replaced when 5-A (library actions) lands with the port of
//! `Services/Library/UpgradeSources.cs`. Only `Ready` (and what it is made of) is here, ported as
//! the C# wrote it, which `HeartOwnership` (4-D) asks before queueing Better quality.

use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::settings::{
    DownloadSource, LidarrSettings, SettingsStore, SoulseekSettings, UpgradeSourceChoice,
};

/// Where Better quality looks: Soulseek, Lidarr, or both in that order.
pub struct UpgradeSources {
    /// `IOptionsMonitor` of the library action, Soulseek and Lidarr settings: read at every use.
    settings: Arc<SettingsStore>,
}

impl UpgradeSources {
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        UpgradeSources { settings }
    }

    /// slskd's address and sign-in are set.
    pub fn soulseek_set_up(settings: &SoulseekSettings) -> bool {
        !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.username.as_deref())
            && !dotnet::is_null_or_white_space(settings.password.as_deref())
    }

    /// Lidarr's address, key and the choices it needs to add an album are set.
    pub fn lidarr_set_up(settings: &LidarrSettings) -> bool {
        !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.api_key.as_deref())
            && !dotnet::is_null_or_white_space(settings.root_folder_path.as_deref())
            && settings.quality_profile_id > 0
            && settings.metadata_profile_id > 0
    }

    /// The sources the setting allows, in the order they are tried, set up or not.
    pub fn wanted(&self) -> Vec<DownloadSource> {
        match self.settings.current().library_actions.upgrade_source {
            UpgradeSourceChoice::Soulseek => vec![DownloadSource::Soulseek],
            UpgradeSourceChoice::Lidarr => vec![DownloadSource::Lidarr],
            _ => vec![DownloadSource::Soulseek, DownloadSource::Lidarr],
        }
    }

    /// The allowed sources that are set up, in the order they are tried.
    pub fn plan(&self) -> Vec<DownloadSource> {
        self.wanted().into_iter().filter(|s| self.set_up(*s)).collect()
    }

    /// Whether any allowed source is set up, so Better quality can be offered at all.
    pub fn ready(&self) -> bool {
        !self.plan().is_empty()
    }

    fn set_up(&self, source: DownloadSource) -> bool {
        let settings = self.settings.current();
        match source {
            DownloadSource::Soulseek => Self::soulseek_set_up(&settings.soulseek),
            DownloadSource::Lidarr => Self::lidarr_set_up(&settings.lidarr),
            _ => false,
        }
    }
}
