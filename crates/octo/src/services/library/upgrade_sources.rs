//! Port of `Services/Library/UpgradeSources.cs`.

use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::settings::{
    DownloadSource, LidarrSettings, SettingsStore, SoulseekSettings, UpgradeSourceChoice,
};

use crate::services::soulseek::ISoulseekLink;
use crate::services::soulseek::soulseek_link::SoulseekLinkState;

/// Where Better quality looks for a lossless copy, decided in one place so the dashboard, the
/// apps, the queue and the weekly worker never disagree. Only lossless sources count: YouTube can
/// only produce an MP3, which a Better quality check refuses anyway.
pub struct UpgradeSources {
    /// `IOptionsMonitor` of the library action, Soulseek and Lidarr settings: read at every use.
    settings: Arc<SettingsStore>,
    soulseek_link: Option<Arc<dyn ISoulseekLink>>,
}

impl UpgradeSources {
    pub fn new(settings: Arc<SettingsStore>, soulseek_link: Option<Arc<dyn ISoulseekLink>>) -> Self {
        UpgradeSources {
            settings,
            soulseek_link,
        }
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

    /// The sources in words, for "Looking for a higher quality copy on X": the ones that are set
    /// up, or, when none is, the ones the setting asks for.
    pub fn name(&self) -> String {
        let plan = self.plan();
        Self::words(if plan.is_empty() { self.wanted() } else { plan })
    }

    /// The sources that can take a search right now. Soulseek drops out while slskd is not
    /// logged in; empty means every source is out, and the request should wait rather than fail.
    pub async fn available(&self) -> Vec<DownloadSource> {
        let plan = self.plan();
        let Some(link) = self.soulseek_link.as_ref() else {
            return plan;
        };
        if !plan.contains(&DownloadSource::Soulseek) {
            return plan;
        }
        let state = link.read(false).await.map(|reading| reading.link);
        if state == Some(SoulseekLinkState::NotLoggedIn) {
            plan.into_iter()
                .filter(|s| *s != DownloadSource::Soulseek)
                .collect()
        } else {
            plan
        }
    }

    /// Whether the only thing between this request and a search is a Soulseek outage.
    pub async fn waiting_for_soulseek(&self) -> bool {
        self.plan().contains(&DownloadSource::Soulseek) && self.available().await.is_empty()
    }

    pub fn words(sources: impl IntoIterator<Item = DownloadSource>) -> String {
        let mut words: Vec<&str> = Vec::new();
        for source in sources {
            let word = Self::word(source);
            if !words.contains(&word) {
                words.push(word);
            }
        }
        words.join(" or ")
    }

    pub fn word(source: DownloadSource) -> &'static str {
        match source {
            DownloadSource::Lidarr => "Lidarr",
            DownloadSource::YouTube => "YouTube",
            _ => "Soulseek",
        }
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

#[cfg(test)]
mod tests {
    //! Port of `UpgradeSourcesTests`. Where Better quality looks for a lossless copy: Soulseek,
    //! Lidarr, or both in that order. One place decides it, so the dashboard, the apps and the
    //! workers never disagree. Only sources that are set up count, and a Soulseek outage leaves
    //! Lidarr rather than stopping everything.

    use async_trait::async_trait;
    use chrono::{DateTime, TimeDelta, Utc};
    use octo_core::settings::{AppSettings, LibraryActionSettings};

    use super::*;
    use crate::services::soulseek::soulseek_link::SoulseekServerReading;

    fn slskd() -> SoulseekSettings {
        SoulseekSettings {
            base_url: Some("http://slskd:5030".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        }
    }

    fn lidarr() -> LidarrSettings {
        LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            root_folder_path: Some("/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        }
    }

    struct FixedLink(SoulseekLinkState);

    #[async_trait]
    impl ISoulseekLink for FixedLink {
        async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
            Some(SoulseekServerReading::new(self.0, None, None, None))
        }

        fn hold_limit(&self) -> TimeDelta {
            TimeDelta::zero()
        }

        fn utc_now(&self) -> DateTime<Utc> {
            Utc::now()
        }

        async fn wait_for_login(&self, _until: DateTime<Utc>) -> bool {
            true
        }
    }

    fn sources(
        choice: UpgradeSourceChoice,
        soulseek: Option<SoulseekSettings>,
        lidarr: Option<LidarrSettings>,
        link: SoulseekLinkState,
    ) -> UpgradeSources {
        UpgradeSources::new(
            Arc::new(SettingsStore::for_tests(AppSettings {
                library_actions: LibraryActionSettings {
                    upgrade_source: choice,
                    ..Default::default()
                },
                soulseek: soulseek.unwrap_or_default(),
                lidarr: lidarr.unwrap_or_default(),
                ..Default::default()
            })),
            Some(Arc::new(FixedLink(link))),
        )
    }

    fn logged_in(
        choice: UpgradeSourceChoice,
        soulseek: Option<SoulseekSettings>,
        lidarr: Option<LidarrSettings>,
    ) -> UpgradeSources {
        sources(choice, soulseek, lidarr, SoulseekLinkState::LoggedIn)
    }

    #[test]
    fn auto_tries_soulseek_then_lidarr() {
        let sources = logged_in(UpgradeSourceChoice::Auto, Some(slskd()), Some(lidarr()));

        assert_eq!(sources.plan(), [DownloadSource::Soulseek, DownloadSource::Lidarr]);
        assert!(sources.ready());
        assert_eq!(sources.name(), "Soulseek or Lidarr");
    }

    #[test]
    fn auto_uses_only_what_is_set_up() {
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, None, Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, None, Some(lidarr())).name(),
            "Lidarr"
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, Some(slskd()), None).plan(),
            [DownloadSource::Soulseek]
        );
    }

    #[test]
    fn a_chosen_source_is_the_only_one() {
        assert_eq!(
            logged_in(UpgradeSourceChoice::Lidarr, Some(slskd()), Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Soulseek, Some(slskd()), Some(lidarr())).plan(),
            [DownloadSource::Soulseek]
        );
    }

    #[test]
    fn nothing_set_up_is_not_ready_and_names_what_it_wants() {
        let sources = logged_in(UpgradeSourceChoice::Lidarr, Some(slskd()), None);

        assert!(sources.plan().is_empty());
        assert!(!sources.ready());
        assert_eq!(sources.name(), "Lidarr");
    }

    #[test]
    fn lidarr_needs_its_root_folder_and_profiles_to_add_an_album() {
        let half = LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            ..Default::default()
        };

        assert!(!UpgradeSources::lidarr_set_up(&half));
        assert!(UpgradeSources::lidarr_set_up(&lidarr()));
    }

    #[tokio::test]
    async fn a_soulseek_outage_leaves_lidarr() {
        let sources = sources(
            UpgradeSourceChoice::Auto,
            Some(slskd()),
            Some(lidarr()),
            SoulseekLinkState::NotLoggedIn,
        );

        assert_eq!(sources.available().await, [DownloadSource::Lidarr]);
        assert!(!sources.waiting_for_soulseek().await);
    }

    #[tokio::test]
    async fn with_only_soulseek_an_outage_means_wait() {
        let sources = sources(
            UpgradeSourceChoice::Auto,
            Some(slskd()),
            None,
            SoulseekLinkState::NotLoggedIn,
        );

        assert!(sources.available().await.is_empty());
        assert!(sources.waiting_for_soulseek().await);
    }

    #[tokio::test]
    async fn lidarr_alone_never_waits_for_soulseek() {
        let sources = sources(
            UpgradeSourceChoice::Lidarr,
            Some(slskd()),
            Some(lidarr()),
            SoulseekLinkState::NotLoggedIn,
        );

        assert_eq!(sources.available().await, [DownloadSource::Lidarr]);
        assert!(!sources.waiting_for_soulseek().await);
    }
}
