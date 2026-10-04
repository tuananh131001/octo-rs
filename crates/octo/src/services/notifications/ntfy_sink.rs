//! Port of `Services/Notifications/NtfySink.cs`. The topic URL, body and tag are
//! `octo_core::notifications::ntfy_sink`.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::dotnet;
use octo_core::json::{Escaping, Options};
use octo_core::notifications::ntfy_sink::{build_body, parse_topic_url, tag_for};
use octo_core::settings::SettingsStore;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Map, Value, json};

use super::ensure_success_status_code;
use super::i_notification_sink::{INotificationSink, NotificationMessage};

/// ntfy transport, using JSON publishing mode: POST to the server root with the
/// topic in the body, not PUT-to-topic with metadata headers.
///
/// Deliberate: ntfy's header mode carries the title in an HTTP header, and .NET's
/// HttpClient rejects non-ASCII header values: for a music app, "Sigur Rós" and
/// "坂本龍一" are the normal case, not the edge case. ntfy documents RFC-2047-encoding
/// each header as the workaround; a UTF-8 JSON body needs none of that. The only
/// header left is the ASCII Bearer token.
pub struct NtfySink {
    /// The "notifications" named client.
    http: reqwest::Client,
    /// `IOptionsMonitor<NotificationSettings>`: read at every use.
    settings: Arc<SettingsStore>,
}

impl NtfySink {
    pub fn new(http: reqwest::Client, settings: Arc<SettingsStore>) -> Self {
        Self { http, settings }
    }
}

#[async_trait]
impl INotificationSink for NtfySink {
    fn name(&self) -> &str {
        "ntfy"
    }

    fn is_configured(&self) -> bool {
        !dotnet::is_blank(&self.settings.current().notifications.ntfy_url)
    }

    async fn send(&self, message: &NotificationMessage) -> anyhow::Result<()> {
        let settings = self.settings.current().notifications.clone();
        let (server_root, topic) = parse_topic_url(&settings.ntfy_url)?;

        let mut payload = Map::new();
        payload.insert("topic".into(), json!(topic));
        payload.insert("title".into(), json!(message.title));
        payload.insert("message".into(), json!(build_body(message)));
        payload.insert("tags".into(), json!([tag_for(message.r#type)]));
        if let Some(image) = message.image_url.as_deref().filter(|url| !url.is_empty()) {
            // Both slots on purpose: icon is what the phone shows in the
            // notification shade next to the text, attach is the full-size art
            // when the notification is expanded.
            payload.insert("icon".into(), json!(image));
            payload.insert("attach".into(), json!(image));
        }

        // Literal UTF-8 in the body rather than \uXXXX escapes. Both decode the same,
        // but the un-escaped form is what a human sees tailing the wire, and carrying
        // "Sigur Rós" readably is the whole reason this sink uses JSON mode.
        let body = octo_core::json::to_string_with(
            &Value::Object(payload),
            Options {
                escaping: Escaping::Relaxed,
                indented: false,
            },
        );
        let mut request = self
            .http
            .post(&server_root)
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body);
        if !dotnet::is_blank(&settings.ntfy_token) {
            request = request.header(AUTHORIZATION, format!("Bearer {}", settings.ntfy_token));
        }

        let response = request.send().await?;
        ensure_success_status_code(response.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::test_support::received;
    use octo_core::notifications::NotificationEventType;
    use octo_core::settings::{AppSettings, NotificationSettings};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    async fn build(topic_path: &str, token: &str) -> (NtfySink, MockServer) {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            notifications: NotificationSettings {
                ntfy_url: format!("{}{topic_path}", server.uri()),
                ntfy_token: token.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }));
        (NtfySink::new(reqwest::Client::new(), settings), server)
    }

    fn message(title: &str, image: Option<&str>) -> NotificationMessage {
        NotificationMessage::new(NotificationEventType::DownloadCompleted, title, "b", image)
    }

    async fn last(server: &MockServer) -> Request {
        received(server).await.pop().expect("a request")
    }

    #[tokio::test]
    async fn publishes_utf8_json_not_headers() {
        let (sink, server) = build("/octo", "").await;

        sink.send(&message("Sigur Rós \u{2013} Ágætis byrjun", None))
            .await
            .expect("sent");

        // The whole reason for JSON mode: the title travels in the UTF-8 body and
        // never as an HTTP header.
        let request = last(&server).await;
        let body = String::from_utf8_lossy(&request.body).into_owned();
        assert!(body.contains("Sigur Rós"), "{body}");
        assert!(request.headers.get("title").is_none());
        assert!(body.contains(r#""topic":"octo""#), "{body}");
        assert_eq!(request.url.path(), "/");
    }

    #[tokio::test]
    async fn bearer_token_only_when_configured() {
        let (bare, bare_server) = build("/octo", "").await;
        bare.send(&message("t", None)).await.expect("sent");
        assert!(last(&bare_server).await.headers.get("authorization").is_none());

        let (authed, authed_server) = build("/octo", "tk_secret").await;
        authed.send(&message("t", None)).await.expect("sent");
        assert_eq!(
            last(&authed_server)
                .await
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tk_secret")
        );
    }

    #[tokio::test]
    async fn cover_art_becomes_icon_and_attach_and_absent_means_no_keys() {
        let (sink, server) = build("/octo", "").await;

        // Icon shows in the notification shade, attach when expanded: art in
        // both places is the "really nice" ask for the phone side.
        sink.send(&message("t", Some("https://cdn.example/cover.jpg")))
            .await
            .expect("sent");
        let body = String::from_utf8_lossy(&last(&server).await.body).into_owned();
        assert!(
            body.contains(r#""attach":"https://cdn.example/cover.jpg""#),
            "{body}"
        );
        assert!(
            body.contains(r#""icon":"https://cdn.example/cover.jpg""#),
            "{body}"
        );

        sink.send(&message("t", None)).await.expect("sent");
        let body = String::from_utf8_lossy(&last(&server).await.body).into_owned();
        assert!(!body.contains("attach"), "{body}");
        assert!(!body.contains("icon"), "{body}");
    }

    /// Rust-only: the whole body, and a failure the test button shows as .NET worded it.
    #[tokio::test]
    async fn the_body_and_a_refusal_read_as_the_csharp_wrote_them() {
        let (sink, server) = build("/ntfy/octo", "").await;
        sink.send(&message("A \u{2013} B", None)).await.expect("sent");
        let request = last(&server).await;
        assert_eq!(request.url.path(), "/ntfy");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            "{\"topic\":\"octo\",\"title\":\"A \u{2013} B\",\"message\":\"b\",\"tags\":[\"white_check_mark\"]}"
        );

        let refusing = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&refusing)
            .await;
        let sink = NtfySink::new(
            reqwest::Client::new(),
            Arc::new(SettingsStore::for_tests(AppSettings {
                notifications: NotificationSettings {
                    ntfy_url: format!("{}/octo", refusing.uri()),
                    ..Default::default()
                },
                ..Default::default()
            })),
        );
        assert!(sink.is_configured());
        assert_eq!(
            sink.send(&message("t", None))
                .await
                .expect_err("refused")
                .to_string(),
            "Response status code does not indicate success: 404 (Not Found)."
        );
    }
}
