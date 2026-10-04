//! Port of `Services/Notifications/INotificationSink.cs`: the transport trait. Its records
//! (`NotificationMessage`, `NotificationTestResult`) are `octo_core::notifications`.

use async_trait::async_trait;
pub use octo_core::notifications::{NotificationMessage, NotificationTestResult};

/// One notification transport. Sinks MAY fail from `send`: the orchestrator owns the catch
/// for pipeline events, and the admin test endpoint wants the real error text rather than a
/// swallowed failure.
#[async_trait]
pub trait INotificationSink: Send + Sync {
    /// Stable short name for logs and the test endpoint ("ntfy", "discord").
    fn name(&self) -> &str;

    /// True when this sink's URL is non-empty in current settings. Reads the settings at call
    /// time, so toggling in the admin UI applies without a restart.
    fn is_configured(&self) -> bool;

    async fn send(&self, message: &NotificationMessage) -> anyhow::Result<()>;
}
