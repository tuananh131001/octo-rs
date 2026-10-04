//! Port of `Services/Soulseek/SoulseekStartupValidator.cs`.

use async_trait::async_trait;
use octo_core::settings::SettingsStore;
use octo_core::validation::{BaseStartupValidator, ConsoleColor, IStartupValidator, ValidationResult};
use tokio_util::sync::CancellationToken;

use super::soulseek_client::SoulseekClient;

/// Validates that slskd is reachable + that the music source is wired correctly.
pub struct SoulseekStartupValidator {
    /// `IOptions<SoulseekSettings>`: deliberately the values Octo started with.
    base_url: Option<String>,
    search_wait_seconds: i32,
    min_file_size_bytes: i64,
    client: SoulseekClient,
    /// `configuration["Library:DownloadPath"] ?? "/music"`, read once here.
    download_path: String,
}

impl SoulseekStartupValidator {
    pub fn new(settings: &SettingsStore, client: SoulseekClient) -> Self {
        let soulseek = settings.current().soulseek.clone();
        SoulseekStartupValidator {
            base_url: soulseek.base_url,
            search_wait_seconds: soulseek.search_wait_seconds,
            min_file_size_bytes: soulseek.min_file_size_bytes,
            client,
            download_path: settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "/music".to_string()),
        }
    }

    fn normalize(path: &str) -> String {
        path.replace('\\', "/").trim_end_matches('/').to_string()
    }
}

#[async_trait]
impl IStartupValidator for SoulseekStartupValidator {
    fn service_name(&self) -> &str {
        "Soulseek"
    }

    async fn validate(&self, _cancellation_token: &CancellationToken) -> ValidationResult {
        println!();

        let Some(base_url) = self.base_url.as_deref().filter(|u| !u.trim().is_empty()) else {
            BaseStartupValidator::write_status("Soulseek (slskd)", "NOT CONFIGURED", ConsoleColor::Red);
            BaseStartupValidator::write_detail(
                "Set the Soulseek__BaseUrl environment variable (e.g. http://slskd:5030)",
            );
            return ValidationResult::failure("-1", "Soulseek BaseUrl not set", ConsoleColor::Red);
        };

        BaseStartupValidator::write_status("Soulseek BaseUrl", base_url, ConsoleColor::Cyan);
        BaseStartupValidator::write_status(
            "Search wait",
            &format!("{}s", self.search_wait_seconds),
            ConsoleColor::Cyan,
        );
        BaseStartupValidator::write_status(
            "Min file size",
            &format!("{} MB", self.min_file_size_bytes / (1024 * 1024)),
            ConsoleColor::Cyan,
        );

        if !self.client.is_reachable().await {
            BaseStartupValidator::write_status("slskd API", "UNREACHABLE", ConsoleColor::Red);
            BaseStartupValidator::write_detail(
                "Check that slskd is running and Soulseek__BaseUrl + Username + Password are correct",
            );
            return ValidationResult::failure("-1", "slskd unreachable", ConsoleColor::Red);
        }

        BaseStartupValidator::write_status("slskd API", "REACHABLE", ConsoleColor::Green);

        // Diagnostic only: Octo finds finished files by watching its own
        // DownloadPath, so slskd writing anywhere else means downloads
        // "succeed" in slskd but never reach the library (issue #17).
        if let Some(slskd_downloads) = self
            .client
            .get_downloads_directory()
            .await
            .filter(|d| !d.is_empty())
        {
            let normalized = Self::normalize(&slskd_downloads);
            if normalized == Self::normalize(&self.download_path) || normalized == "/music" {
                BaseStartupValidator::write_status(
                    "slskd downloads dir",
                    &slskd_downloads,
                    ConsoleColor::Green,
                );
            } else if octo_core::common::dotnet::to_lower_invariant(&normalized).ends_with("/app/downloads") {
                BaseStartupValidator::write_status(
                    "slskd downloads dir",
                    &format!("{slskd_downloads} (MISCONFIGURED)"),
                    ConsoleColor::Yellow,
                );
                BaseStartupValidator::write_detail(
                    "slskd is writing to its internal default, which Octo cannot see.",
                );
                BaseStartupValidator::write_detail(
                    "Set SLSKD_DOWNLOADS_DIR=/music on the slskd container (or point slskd's",
                );
                BaseStartupValidator::write_detail(&format!(
                    "downloads dir at the same directory as Octo's DownloadPath: {}).",
                    self.download_path
                ));
                BaseStartupValidator::write_detail(
                    "If already set, check slskd.yml for a directories.downloads entry: a",
                );
                BaseStartupValidator::write_detail("yaml value overrides the environment variable.");
            } else {
                // A path we can't judge from inside this container: different
                // mounts can make distinct strings point at the same host dir.
                BaseStartupValidator::write_status(
                    "slskd downloads dir",
                    &slskd_downloads,
                    ConsoleColor::Cyan,
                );
            }
        }

        ValidationResult::success("Soulseek validation passed", None)
    }
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, SoulseekSettings};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn store(base_url: Option<String>) -> SettingsStore {
        SettingsStore::for_tests(AppSettings {
            soulseek: SoulseekSettings {
                base_url,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn validator(settings: &SettingsStore) -> SoulseekStartupValidator {
        SoulseekStartupValidator::new(settings, SoulseekClient::new(&settings.current().soulseek))
    }

    #[tokio::test]
    async fn without_a_base_url_soulseek_is_not_configured() {
        let result = validator(&store(Some(" ".into())))
            .validate(&CancellationToken::new())
            .await;
        assert!(!result.is_valid);
        assert_eq!(result.status, "-1");
        assert_eq!(result.details.as_deref(), Some("Soulseek BaseUrl not set"));
    }

    #[tokio::test]
    async fn an_slskd_that_does_not_answer_is_unreachable() {
        let result = validator(&store(Some("http://127.0.0.1:1".into())))
            .validate(&CancellationToken::new())
            .await;
        assert!(!result.is_valid);
        assert_eq!(result.details.as_deref(), Some("slskd unreachable"));
    }

    #[tokio::test]
    async fn a_reachable_slskd_passes_whatever_its_downloads_directory() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/application"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v0/options"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"directories":{"downloads":"/app/downloads"}}"#),
            )
            .mount(&server)
            .await;
        let settings = store(Some(server.uri()));
        let checker = validator(&settings);
        assert_eq!(checker.service_name(), "Soulseek");
        assert_eq!(checker.download_path, "/music");
        let result = checker.validate(&CancellationToken::new()).await;
        assert!(result.is_valid);
        assert_eq!(result.details.as_deref(), Some("Soulseek validation passed"));
        assert_eq!(SoulseekStartupValidator::normalize(r"C:\music\"), "C:/music");
    }
}
