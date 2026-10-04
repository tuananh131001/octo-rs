//! `Services/Notifications` in the C#: push notifications to ntfy and a Discord webhook. The
//! event, the rendering and the wire shapes are `octo_core::notifications`.

pub mod discord_sink;
pub mod i_notification_sink;
pub mod notification_service;
pub mod ntfy_sink;

pub use discord_sink::DiscordSink;
pub use i_notification_sink::INotificationSink;
pub use notification_service::NotificationService;
pub use ntfy_sink::NtfySink;

/// `HttpResponseMessage.EnsureSuccessStatusCode()`, with its message: the admin test button
/// shows it verbatim.
pub(crate) fn ensure_success_status_code(status: reqwest::StatusCode) -> anyhow::Result<()> {
    if status.is_success() {
        return Ok(());
    }
    anyhow::bail!(
        "Response status code does not indicate success: {} ({}).",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
}
