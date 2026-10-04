//! The records of `Services/Notifications/INotificationSink.cs`. The sink trait itself is async
//! and lives with the transports in `octo::services::notifications::i_notification_sink`.

use serde::Serialize;

use super::notification_event::NotificationEventType;

/// Transport-agnostic rendered content, produced once per event.
///
/// Body is the complete plain-text rendering and is what text-first transports
/// (ntfy) send. Description and Fields exist for transports with layout: when
/// Fields is set, Discord builds a song card (description line, inline
/// stat fields, full-width art) instead of repeating the Body prose. Both forms
/// are produced by the same render call from the same event, so the transports
/// never disagree on facts, only on formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationMessage {
    pub r#type: NotificationEventType,
    pub title: String,
    pub body: String,
    pub image_url: Option<String>,
    pub description: Option<String>,
    pub fields: Option<Vec<(String, String)>>,
}

impl NotificationMessage {
    /// `new NotificationMessage(type, title, body, imageUrl)`: no description, no fields.
    pub fn new(
        r#type: NotificationEventType,
        title: impl Into<String>,
        body: impl Into<String>,
        image_url: Option<&str>,
    ) -> Self {
        Self {
            r#type,
            title: title.into(),
            body: body.into(),
            image_url: image_url.map(str::to_string),
            description: None,
            fields: None,
        }
    }
}

/// Per-sink outcome of the admin "Send test" button. Serialised as ASP.NET wrote the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationTestResult {
    pub sink: String,
    pub configured: bool,
    pub ok: bool,
    pub detail: String,
}
