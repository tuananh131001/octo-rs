//! Notifications, the pure parts (`Services/Notifications`): the event, the rendered message,
//! and each transport's wire shape. The sinks and the fan-out are
//! `octo::services::notifications`.

pub mod discord_sink;
pub mod i_notification_sink;
pub mod notification_event;
pub mod notification_service;
pub mod ntfy_sink;

pub use i_notification_sink::{NotificationMessage, NotificationTestResult};
pub use notification_event::{NotificationEvent, NotificationEventType};
