//! Port of `Services/ListenBrainz/ListenBrainzService.cs`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::{dotnet, octo_user_agent};
use octo_core::json::element::{get_boolean, get_string, try_get_property};
use octo_core::settings::SettingsStore;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;
use crate::services::http_client_factory::{connect_failure_message, timeout_message};

/// `ValidateTokenAsync`'s answer: whether the token is valid, whose it is, and what to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenValidation {
    pub valid: bool,
    pub user_name: Option<String>,
    pub detail: String,
}

impl TokenValidation {
    fn refused(detail: impl Into<String>) -> Self {
        Self {
            valid: false,
            user_name: None,
            detail: detail.into(),
        }
    }
}

/// Submits listens to ListenBrainz for plays of external tracks. One call per
/// completed play, best effort: a failure is logged and never reaches the client,
/// because a listen is a record of something that already happened.
pub struct ListenBrainzService {
    http: reqwest::Client,
    submit_url: String,
    validate_url: String,
    /// `IOptionsMonitor<ListenBrainzSettings>`: read at every use.
    settings: Arc<SettingsStore>,
    version: &'static str,
}

impl ListenBrainzService {
    pub const CLIENT_NAME: &'static str = "listenbrainz";
    pub const API_BASE: &'static str = "https://api.listenbrainz.org";

    /// Listens are records of plays that already happened; a slow ListenBrainz must not
    /// hold a scrobble response or a radio stream, so the client is short-fused.
    pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(settings: Arc<SettingsStore>) -> Self {
        let http = client_builder()
            .timeout(Self::CLIENT_TIMEOUT)
            .build()
            .expect("the ListenBrainz client builds");
        Self::with_parts(http, Self::API_BASE, settings)
    }

    /// A service that talks to another host: a test's mock server. `api_base` is the part
    /// before `/1/submit-listens`.
    pub fn with_parts(http: reqwest::Client, api_base: &str, settings: Arc<SettingsStore>) -> Self {
        let base = api_base.trim_end_matches('/');
        Self {
            http,
            submit_url: format!("{base}/1/submit-listens"),
            validate_url: format!("{base}/1/validate-token"),
            settings,
            // The assembly's informational version, without its +sha.
            version: octo_user_agent::version(),
        }
    }

    /// True when this listener has a token and submission is switched on.
    pub fn is_enabled_for(&self, username: &str) -> bool {
        self.settings
            .current()
            .listen_brainz
            .token_for(username)
            .is_some()
    }

    /// Records one completed play. Returns true when ListenBrainz accepted it, false
    /// when nothing was sent (no token) or the submission failed.
    pub async fn submit_listen(
        &self,
        username: &str,
        artist: &str,
        title: &str,
        album: Option<&str>,
        duration_seconds: Option<i32>,
        listened_at_utc: DateTime<Utc>,
    ) -> bool {
        let Some(token) = self.settings.current().listen_brainz.token_for(username) else {
            return false;
        };
        if dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return false;
        }

        let mut additional = Map::new();
        additional.insert("media_player".into(), json!("Octo"));
        additional.insert("submission_client".into(), json!("Octo"));
        additional.insert("submission_client_version".into(), json!(self.version));
        if let Some(duration) = duration_seconds.filter(|d| *d > 0) {
            // An int multiplication, unchecked as the C# was.
            additional.insert("duration_ms".into(), json!(duration.wrapping_mul(1000)));
        }
        let mut metadata = Map::new();
        metadata.insert("artist_name".into(), json!(artist.trim()));
        metadata.insert("track_name".into(), json!(title.trim()));
        metadata.insert("additional_info".into(), Value::Object(additional));
        if let Some(album) = album.filter(|a| !dotnet::is_blank(a)) {
            metadata.insert("release_name".into(), json!(album.trim()));
        }
        let body = octo_core::json::to_string(&json!({
            "listen_type": "single",
            "payload": [{
                "listened_at": listened_at_utc.timestamp(),
                "track_metadata": Value::Object(metadata),
            }],
        }));

        let sent = self
            .http
            .post(&self.submit_url)
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .header(AUTHORIZATION, format!("Token {token}"))
            .body(body)
            .send()
            .await;
        let answer = match sent {
            Ok(response) => HttpAnswer::read(response).await,
            Err(e) => Err(e),
        };
        match answer {
            Ok(answer) if answer.is_success() => {
                info!("ListenBrainz listen submitted for {username}: {artist} - {title}");
                true
            }
            Ok(answer) => {
                warn!(
                    "ListenBrainz rejected a listen for {username} with HTTP {}: {}",
                    answer.status.as_u16(),
                    answer.text().trim()
                );
                false
            }
            Err(e) => {
                warn!(error = %failure_message(&e), "ListenBrainz submission failed for {username}");
                false
            }
        }
    }

    /// Asks ListenBrainz whether a token is valid and whose it is.
    pub async fn validate_token(&self, token: &str) -> TokenValidation {
        if dotnet::is_blank(token) {
            return TokenValidation::refused("No token configured.");
        }
        let sent = self
            .http
            .get(&self.validate_url)
            .header(AUTHORIZATION, format!("Token {}", token.trim()))
            .send()
            .await;
        let answer = match sent {
            Ok(response) => HttpAnswer::read(response).await,
            Err(e) => Err(e),
        };
        let answer = match answer {
            Ok(answer) => answer,
            Err(e) => return TokenValidation::refused(failure_message(&e)),
        };
        if !answer.is_success() {
            return TokenValidation::refused(format!("HTTP {}", answer.status.as_u16()));
        }
        let read = || -> anyhow::Result<TokenValidation> {
            let root = answer.json()?;
            let valid = match try_get_property(&root, "valid")? {
                Some(element) => get_boolean(element)?,
                None => false,
            };
            let user = match try_get_property(&root, "user_name")? {
                Some(element) => get_string(element)?.map(str::to_string),
                None => None,
            };
            let detail = if valid {
                format!("Valid, belongs to {}.", user.as_deref().unwrap_or(""))
            } else {
                "ListenBrainz says this token is not valid.".to_string()
            };
            Ok(TokenValidation {
                valid,
                user_name: user,
                detail,
            })
        };
        read().unwrap_or_else(|e| TokenValidation::refused(e.to_string()))
    }
}

/// The `Message` of the exception a failed call threw.
fn failure_message(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        timeout_message(ListenBrainzService::CLIENT_TIMEOUT)
    } else {
        connect_failure_message(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::test_support::received;
    use chrono::TimeZone;
    use octo_core::settings::{AppSettings, ListenBrainzSettings};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn service(server: &MockServer, settings: ListenBrainzSettings) -> ListenBrainzService {
        ListenBrainzService::with_parts(
            reqwest::Client::new(),
            &server.uri(),
            Arc::new(SettingsStore::for_tests(AppSettings {
                listen_brainz: settings,
                ..Default::default()
            })),
        )
    }

    fn tokens() -> ListenBrainzSettings {
        ListenBrainzSettings {
            token: "default-token".into(),
            user_tokens: [("Alice".to_string(), " alice-token ".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }

    /// Rust-only (no C# test drives the service directly): the listen as ListenBrainz is sent
    /// it, with the listener's own token.
    #[tokio::test]
    async fn a_listen_is_submitted_with_the_listeners_token_and_every_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/1/submit-listens"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
        let service = service(&server, tokens());
        let at = Utc
            .with_ymd_and_hms(2026, 10, 4, 12, 0, 0)
            .single()
            .expect("a time");

        assert!(service.is_enabled_for("alice"));
        assert!(
            service
                .submit_listen(
                    "alice",
                    " Sigur Rós ",
                    "Hoppípolla",
                    Some("Takk..."),
                    Some(268),
                    at
                )
                .await
        );

        let requests = received(&server).await;
        let request = requests.first().expect("one request");
        assert_eq!(
            request.headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Token alice-token")
        );
        assert_eq!(
            request.headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            format!(
                r#"{{"listen_type":"single","payload":[{{"listened_at":{},"track_metadata":{{"artist_name":"Sigur R\u00F3s","track_name":"Hopp\u00EDpolla","additional_info":{{"media_player":"Octo","submission_client":"Octo","submission_client_version":"{}","duration_ms":268000}},"release_name":"Takk..."}}}}]}}"#,
                at.timestamp(),
                octo_user_agent::version()
            )
        );
    }

    /// Rust-only: nothing is sent without a token or a song, and a refusal is false.
    #[tokio::test]
    async fn nothing_goes_without_a_token_and_a_refusal_is_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string(" bad token "))
            .mount(&server)
            .await;
        let off = service(
            &server,
            ListenBrainzSettings {
                submit_external_plays: false,
                ..tokens()
            },
        );
        assert!(!off.is_enabled_for("alice"));
        assert!(!off.submit_listen("alice", "A", "T", None, None, Utc::now()).await);
        let on = service(&server, tokens());
        assert!(!on.submit_listen("bob", " ", "T", None, None, Utc::now()).await);
        assert!(received(&server).await.is_empty());

        assert!(!on.submit_listen("bob", "A", "T", None, Some(0), Utc::now()).await);
        let requests = received(&server).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Token default-token")
        );
        assert!(!String::from_utf8_lossy(&requests[0].body).contains("duration_ms"));
        assert!(!String::from_utf8_lossy(&requests[0].body).contains("release_name"));
    }

    /// Rust-only: the dashboard's token check and its texts.
    #[tokio::test]
    async fn validate_token_says_whose_it_is() {
        let server = MockServer::start().await;
        Mock::given(path("/1/validate-token"))
            .and(wiremock::matchers::header("authorization", "Token good"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"valid":true,"user_name":"alice_lb"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(path("/1/validate-token"))
            .and(wiremock::matchers::header("authorization", "Token stale"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"valid":false}"#))
            .mount(&server)
            .await;
        Mock::given(path("/1/validate-token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let service = service(&server, tokens());

        assert_eq!(
            service.validate_token(" good ").await,
            TokenValidation {
                valid: true,
                user_name: Some("alice_lb".into()),
                detail: "Valid, belongs to alice_lb.".into()
            }
        );
        assert_eq!(
            service.validate_token("stale").await.detail,
            "ListenBrainz says this token is not valid."
        );
        assert_eq!(service.validate_token("other").await.detail, "HTTP 401");
        assert_eq!(service.validate_token("  ").await.detail, "No token configured.");
    }
}
