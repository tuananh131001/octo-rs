//! Port of `Services/Validation/SubsonicStartupValidator.cs`.

use async_trait::async_trait;

use super::base_startup_validator::{write_detail, write_status};
use super::{ConsoleColor, IStartupValidator, ValidationResult};
use crate::services::http_client_factory::connect_failure_message;

/// Validates Subsonic server connectivity at startup
pub struct SubsonicStartupValidator {
    /// `IOptions<SubsonicSettings>.Value.Url`: deliberately the value Octo started with.
    url: Option<String>,
    http: reqwest::Client,
}

impl SubsonicStartupValidator {
    pub fn new(url: Option<String>, http: reqwest::Client) -> Self {
        SubsonicStartupValidator { url, http }
    }
}

#[async_trait]
impl IStartupValidator for SubsonicStartupValidator {
    fn service_name(&self) -> &str {
        "Subsonic"
    }

    async fn validate(&self) -> anyhow::Result<ValidationResult> {
        let Some(subsonic_url) = self.url.as_deref().filter(|u| !u.trim().is_empty()) else {
            write_status("Subsonic URL", "NOT CONFIGURED", ConsoleColor::Red);
            write_detail("Set the Subsonic__Url environment variable");
            return Ok(ValidationResult::not_configured("Subsonic URL not configured"));
        };

        write_status("Subsonic URL", subsonic_url, ConsoleColor::Cyan);

        let ping_url = format!(
            "{}/rest/ping.view?v=1.16.1&c=octo&f=json",
            subsonic_url.trim_end_matches('/')
        );
        let outcome = async {
            let response = self.http.get(&ping_url).send().await?;
            let status = response.status();
            let content = if status.is_success() {
                Some(response.text().await?)
            } else {
                None
            };
            Ok::<_, reqwest::Error>((status, content))
        }
        .await;

        match outcome {
            Ok((_, Some(content))) => {
                if content.contains("\"status\":\"ok\"") || content.contains("status=\"ok\"") {
                    write_status("Subsonic server", "OK", ConsoleColor::Green);
                    Ok(ValidationResult::success("Subsonic server is accessible"))
                } else if content.contains("\"status\":\"failed\"") || content.contains("status=\"failed\"") {
                    write_status("Subsonic server", "REACHABLE", ConsoleColor::Yellow);
                    write_detail("Authentication may be required for some operations");
                    Ok(ValidationResult::success("Subsonic server is reachable"))
                } else {
                    write_status("Subsonic server", "REACHABLE", ConsoleColor::Yellow);
                    write_detail("Unexpected response format");
                    Ok(ValidationResult::success("Subsonic server is reachable"))
                }
            }
            Ok((status, None)) => {
                let code = format!("HTTP {}", status.as_u16());
                write_status("Subsonic server", &code, ConsoleColor::Red);
                Ok(ValidationResult::failure(
                    code,
                    "Subsonic server returned an error",
                    ConsoleColor::Red,
                ))
            }
            Err(error) if error.is_timeout() => {
                write_status("Subsonic server", "TIMEOUT", ConsoleColor::Red);
                write_detail("Could not reach server within 10 seconds");
                Ok(ValidationResult::failure(
                    "TIMEOUT",
                    "Could not reach server within timeout period",
                    ConsoleColor::Red,
                ))
            }
            Err(error) if error.is_connect() || error.is_request() || error.is_body() => {
                let message = connect_failure_message(&error);
                write_status("Subsonic server", "UNREACHABLE", ConsoleColor::Red);
                write_detail(&message);
                Ok(ValidationResult::failure(
                    "UNREACHABLE",
                    message,
                    ConsoleColor::Red,
                ))
            }
            Err(error) => {
                let message = error.to_string();
                write_status("Subsonic server", "ERROR", ConsoleColor::Red);
                write_detail(&message);
                Ok(ValidationResult::failure("ERROR", message, ConsoleColor::Red))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn validate(url: Option<String>) -> ValidationResult {
        SubsonicStartupValidator::new(url, crate::services::http_client_factory::default_client())
            .validate()
            .await
            .expect("no escaped failure")
    }

    #[tokio::test]
    async fn a_ping_answer_decides_the_status() {
        for (body, status, details) in [
            (
                r#"{"subsonic-response":{"status":"ok"}}"#,
                "VALID",
                "Subsonic server is accessible",
            ),
            (
                r#"{"subsonic-response":{"status":"failed"}}"#,
                "VALID",
                "Subsonic server is reachable",
            ),
            ("<html/>", "VALID", "Subsonic server is reachable"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/rest/ping.view"))
                .and(query_param("c", "octo"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            let result = validate(Some(format!("{}/", server.uri()))).await;
            assert_eq!(
                (result.status.as_str(), result.details.as_deref()),
                (status, Some(details)),
                "{body}"
            );
            assert!(result.is_valid);
        }
    }

    #[tokio::test]
    async fn failures_are_named() {
        assert_eq!(
            validate(None).await,
            ValidationResult::not_configured("Subsonic URL not configured")
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let result = validate(Some(server.uri())).await;
        assert_eq!((result.is_valid, result.status.as_str()), (false, "HTTP 503"));

        let result = validate(Some("http://127.0.0.1:1".into())).await;
        assert_eq!(
            (result.status.as_str(), result.details.as_deref()),
            ("UNREACHABLE", Some("Connection refused (127.0.0.1:1)"))
        );
    }
}
