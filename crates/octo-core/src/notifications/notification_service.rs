//! The rendering half of `Services/Notifications/NotificationService.cs`: which events are
//! switched on, and the text every transport carries. The fan-out is
//! `octo::services::notifications::notification_service`.

use super::i_notification_sink::NotificationMessage;
use super::notification_event::{NotificationEvent, NotificationEventType};
use crate::settings::NotificationSettings;

pub fn is_enabled(s: &NotificationSettings, r#type: NotificationEventType) -> bool {
    match r#type {
        NotificationEventType::DownloadStarted => s.notify_download_started,
        NotificationEventType::DownloadCompleted => s.notify_download_completed,
        NotificationEventType::LosslessFallback => s.notify_lossless_fallback,
        NotificationEventType::DownloadFailed => s.notify_download_failed,
        NotificationEventType::AlbumCompleted => s.notify_album_completed,
        // The test button bypasses toggles by design: it verifies transports.
        NotificationEventType::Test => true,
    }
}

/// `$"{value}"` of a nullable: null interpolates as nothing.
fn text<T: ToString>(value: &Option<T>) -> String {
    value.as_ref().map(ToString::to_string).unwrap_or_default()
}

fn is_null_or_empty(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(str::is_empty)
}

/// Single source of truth for the text both transports carry. Track
/// events additionally get structured stat fields so layout-capable transports
/// can render a song card instead of prose.
pub fn render(evt: &NotificationEvent) -> NotificationMessage {
    let track = if is_null_or_empty(&evt.artist) {
        evt.title.clone().unwrap_or_else(|| "unknown track".to_string())
    } else {
        format!("{} \u{2013} {}", text(&evt.artist), text(&evt.title))
    };
    let size = evt.size_bytes.filter(|bytes| *bytes > 0);
    let duration = evt.duration_seconds.filter(|seconds| *seconds > 0);

    match evt.r#type {
        NotificationEventType::DownloadStarted => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Downloading: {track}"),
            body: format!("{} via {}", text(&evt.format), text(&evt.source))
                + &size
                    .map(|sb| format!(" ({})", format_size(sb)))
                    .unwrap_or_default(),
            image_url: evt.cover_art_url.clone(),
            description: evt.album.clone(),
            fields: Some(track_fields(evt)),
        },

        NotificationEventType::DownloadCompleted => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Downloaded: {track}"),
            body: (if is_null_or_empty(&evt.album) {
                String::new()
            } else {
                format!("{}\n", text(&evt.album))
            }) + &format!("{} via {}", text(&evt.format), text(&evt.source))
                + &size
                    .map(|cb| format!(", {}", format_size(cb)))
                    .unwrap_or_default()
                + &duration
                    .map(|ds| format!(" \u{b7} {}", format_duration(ds)))
                    .unwrap_or_default(),
            image_url: evt.cover_art_url.clone(),
            description: evt.album.clone(),
            fields: Some(track_fields(evt)),
        },

        NotificationEventType::LosslessFallback => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Lossless miss: {track}"),
            body: format!(
                "Soulseek failed ({}); settling for YouTube MP3.",
                evt.detail.as_deref().unwrap_or("no usable result")
            ),
            image_url: evt.cover_art_url.clone(),
            description: None,
            fields: None,
        },

        NotificationEventType::DownloadFailed => NotificationMessage {
            r#type: evt.r#type,
            title: format!("Download failed: {track}"),
            body: evt
                .detail
                .clone()
                .unwrap_or_else(|| "Every enabled source failed.".to_string()),
            image_url: evt.cover_art_url.clone(),
            description: None,
            fields: None,
        },

        NotificationEventType::AlbumCompleted => NotificationMessage {
            r#type: evt.r#type,
            // Sent after every album walk, so it has to say when nothing arrived at all.
            title: if evt.track_count == Some(0) && evt.failed_count.is_some_and(|f| f > 0) {
                format!("Album failed: {track}")
            } else {
                format!("Album complete: {track}")
            },
            body: format!(
                "{} tracks fetched, {} lossless",
                text(&evt.track_count),
                text(&evt.lossless_count)
            ) + &evt
                .kept_count
                .filter(|k| *k > 0)
                .map(|k| format!(", {k} already yours"))
                .unwrap_or_default()
                + &evt
                    .upgrading_count
                    .filter(|u| *u > 0)
                    .map(|u| format!(", {u} upgrading"))
                    .unwrap_or_default()
                + &evt
                    .failed_count
                    .filter(|f| *f > 0)
                    .map(|f| format!(", {f} failed"))
                    .unwrap_or_default(),
            image_url: evt.cover_art_url.clone(),
            description: None,
            fields: Some(album_fields(evt)),
        },

        NotificationEventType::Test => NotificationMessage::new(
            evt.r#type,
            "Octo test notification",
            "Transports are wired up correctly.",
            None,
        ),
    }
}

/// The stats a song card shows, in display order, skipping unknowns.
/// Bitrate is derived from size and duration: it describes the actual file,
/// which is the number a lossless-vs-lossy glance wants.
fn track_fields(evt: &NotificationEvent) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut add = |name: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            fields.push((name.to_string(), value));
        }
    };

    add("Format", evt.format.clone());
    add("Source", evt.source.clone());
    let size = evt.size_bytes.filter(|bytes| *bytes > 0);
    if let Some(size) = size {
        add("Size", Some(format_size(size)));
    }
    if let Some(dur) = evt.duration_seconds.filter(|seconds| *seconds > 0) {
        add("Length", Some(format_duration(dur)));
        if let Some(b) = size {
            add(
                "Bitrate",
                Some(format!(
                    "\u{2248}{} kbps",
                    group_thousands(b * 8 / i64::from(dur) / 1000)
                )),
            );
        }
    }
    if let Some(year) = evt.year.filter(|year| *year > 0) {
        add("Year", Some(year.to_string()));
    }
    // Last, because on a single-user library it is always the same name and the
    // fields above are what anyone is actually reading.
    if let Some(askers) = evt.requested_by.as_ref().filter(|askers| !askers.is_empty()) {
        add("Requested by", Some(askers.join(", ")));
    }
    fields
}

fn album_fields(evt: &NotificationEvent) -> Vec<(String, String)> {
    [
        ("Tracks", evt.track_count),
        ("Lossless", evt.lossless_count),
        ("Failed", evt.failed_count),
        ("Already yours", evt.kept_count),
        ("Upgrading", evt.upgrading_count),
    ]
    .into_iter()
    .map(|(name, count)| (name.to_string(), count.unwrap_or(0).to_string()))
    .collect()
}

/// `{n:N0}` with the invariant culture: thousands separated by commas.
fn group_thousands(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 { format!("-{out}") } else { out }
}

/// `F1`/`F0` round half to even on the double's exact value in .NET 9, as Rust's `{:.1}` does.
pub fn format_size(bytes: i64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

pub fn format_duration(seconds: i32) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(r#type: NotificationEventType) -> NotificationEvent {
        NotificationEvent::new(r#type)
    }

    #[test]
    fn started_message_names_the_format_up_front() {
        let msg = render(&NotificationEvent {
            artist: Some("Randy Rogers Band".into()),
            title: Some("In My Arms Instead".into()),
            format: Some("FLAC".into()),
            source: Some("Soulseek".into()),
            size_bytes: Some(34_684_600),
            ..event(NotificationEventType::DownloadStarted)
        });

        // "Did it find lossless or is it settling?" is the event's entire reason
        // to exist, so the answer leads the body.
        assert!(msg.body.contains("FLAC via Soulseek"), "{}", msg.body);
        assert!(msg.body.contains("33.1 MB"), "{}", msg.body);
        assert!(msg.title.starts_with("Downloading:"), "{}", msg.title);
    }

    #[test]
    fn fallback_message_says_what_was_lost() {
        let msg = render(&NotificationEvent {
            artist: Some("A".into()),
            title: Some("B".into()),
            detail: Some("No Soulseek FLAC found".into()),
            ..event(NotificationEventType::LosslessFallback)
        });

        assert!(msg.body.contains("MP3"));
        assert!(msg.body.contains("No Soulseek FLAC found"));
    }

    #[test]
    fn test_event_bypasses_every_toggle() {
        let everything_off = NotificationSettings {
            notify_download_started: false,
            notify_download_completed: false,
            notify_lossless_fallback: false,
            notify_download_failed: false,
            notify_album_completed: false,
            ..Default::default()
        };

        // The test button exists to verify transports; toggles must not mute it.
        assert!(is_enabled(&everything_off, NotificationEventType::Test));
    }

    #[test]
    fn album_summary_counts_tracks_and_losslessness() {
        let msg = render(&NotificationEvent {
            artist: Some("Tame Impala".into()),
            title: Some("Currents".into()),
            track_count: Some(15),
            lossless_count: Some(12),
            failed_count: Some(1),
            ..event(NotificationEventType::AlbumCompleted)
        });

        assert!(
            msg.body.contains("15 tracks fetched, 12 lossless, 1 failed"),
            "{}",
            msg.body
        );
    }

    /// Every track failing used to arrive titled "Album complete".
    #[test]
    fn album_summary_with_nothing_fetched_says_failed() {
        let msg = render(&NotificationEvent {
            artist: Some("Tame Impala".into()),
            title: Some("Currents".into()),
            track_count: Some(0),
            lossless_count: Some(0),
            failed_count: Some(12),
            ..event(NotificationEventType::AlbumCompleted)
        });

        assert!(msg.title.starts_with("Album failed:"), "{}", msg.title);
    }

    /// The heart chain has one to three sources, so "Both" was wrong for most setups.
    #[test]
    fn failure_without_detail_does_not_assume_two_sources() {
        let msg = render(&NotificationEvent {
            artist: Some("A".into()),
            title: Some("B".into()),
            ..event(NotificationEventType::DownloadFailed)
        });

        assert!(!msg.body.contains("Both"));
    }

    /// Rust-only: the exact texts, checked against the C# Render.
    #[test]
    fn rendered_texts_match_the_csharp_word_for_word() {
        let completed = render(&NotificationEvent {
            artist: Some("Sigur Rós".into()),
            title: Some("Hoppípolla".into()),
            album: Some("Takk...".into()),
            format: Some("FLAC".into()),
            source: Some("Soulseek".into()),
            size_bytes: Some(34_684_600),
            duration_seconds: Some(4 * 60 + 28),
            year: Some(2005),
            requested_by: Some(vec!["alice".into(), "bob".into()]),
            ..event(NotificationEventType::DownloadCompleted)
        });
        assert_eq!(completed.title, "Downloaded: Sigur Rós \u{2013} Hoppípolla");
        assert_eq!(completed.body, "Takk...\nFLAC via Soulseek, 33.1 MB \u{b7} 4:28");
        assert_eq!(completed.description.as_deref(), Some("Takk..."));
        assert_eq!(
            completed.fields.expect("a song card"),
            [
                ("Format", "FLAC"),
                ("Source", "Soulseek"),
                ("Size", "33.1 MB"),
                ("Length", "4:28"),
                ("Bitrate", "\u{2248}1,035 kbps"),
                ("Year", "2005"),
                ("Requested by", "alice, bob"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
        );

        let bare = render(&event(NotificationEventType::DownloadStarted));
        assert_eq!(bare.title, "Downloading: unknown track");
        assert_eq!(bare.body, " via ");
        assert_eq!(bare.fields, Some(Vec::new()));

        let album = render(&NotificationEvent {
            title: Some("Currents".into()),
            kept_count: Some(2),
            upgrading_count: Some(1),
            ..event(NotificationEventType::AlbumCompleted)
        });
        assert_eq!(album.title, "Album complete: Currents");
        assert_eq!(
            album.body,
            " tracks fetched,  lossless, 2 already yours, 1 upgrading"
        );
        assert_eq!(
            album.fields.expect("counts"),
            [
                ("Tracks", "0"),
                ("Lossless", "0"),
                ("Failed", "0"),
                ("Already yours", "2"),
                ("Upgrading", "1")
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
        );

        let test = render(&event(NotificationEventType::Test));
        assert_eq!(
            (test.title.as_str(), test.body.as_str(), test.image_url),
            (
                "Octo test notification",
                "Transports are wired up correctly.",
                None
            )
        );
    }

    /// Rust-only: sizes and lengths as .NET 9 formats them (`F1` rounds half to even).
    #[test]
    fn sizes_and_durations_format_as_dotnet() {
        assert_eq!(format_size(512), "0 KB");
        assert_eq!(format_size(1536), "2 KB");
        assert_eq!(format_size(2560), "2 KB");
        assert_eq!(format_size(1_310_720), "1.2 MB");
        assert_eq!(format_size(34_684_600), "33.1 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(223), "3:43");
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(3725), "1:02:05");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
    }
}
