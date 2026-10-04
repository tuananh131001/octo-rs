//! Port of `Services/Notifications/NotificationEvent.cs`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationEventType {
    DownloadStarted,
    DownloadCompleted,
    LosslessFallback,
    DownloadFailed,
    AlbumCompleted,

    /// The admin "Send test" button. Always allowed regardless of the
    /// per-event toggles; never fired by the download pipeline.
    Test,
}

/// One thing that happened, in domain terms. Rendering to title/body text happens
/// once in [`render`](super::notification_service::render) so every transport says the
/// same thing. All fields except the type are optional on purpose: events fire from
/// paths where metadata may be partial, and a missing field must degrade the text,
/// never the send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEvent {
    pub r#type: NotificationEventType,

    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,

    /// "FLAC" / "MP3" / "M4A".
    pub format: Option<String>,

    /// "Soulseek" / "YouTube".
    pub source: Option<String>,

    pub cover_art_url: Option<String>,

    /// For DownloadStarted this is the chosen candidate's advertised size;
    /// DownloadCompleted carries the real file's.
    pub size_bytes: Option<i64>,

    /// Track length in seconds. On the completed path this is the enriched
    /// song's duration (EnrichAsync runs before the hook); on started it is
    /// the routing's expected duration.
    pub duration_seconds: Option<i32>,

    pub year: Option<i32>,

    /// Fallback reason or failure message.
    pub detail: Option<String>,

    // AlbumCompleted only. Counts cover the walked tracks; the track whose star
    // triggered the walk got its own DownloadCompleted.
    pub track_count: Option<i32>,
    pub lossless_count: Option<i32>,
    pub failed_count: Option<i32>,

    /// Songs of the album already in the library, kept as they were.
    pub kept_count: Option<i32>,

    /// Owned songs that were lossy, queued for a higher quality copy instead.
    pub upgrading_count: Option<i32>,

    /// Who asked for this, when Octo could tell. Empty for an acquisition Octo started
    /// itself, and for every event when the setting is off.
    pub requested_by: Option<Vec<String>>,
}

impl NotificationEvent {
    /// `new NotificationEvent { Type = type }`: every other field unset. Fill the rest with
    /// `NotificationEvent { artist: .., ..NotificationEvent::new(type) }`.
    pub fn new(r#type: NotificationEventType) -> Self {
        Self {
            r#type,
            artist: None,
            title: None,
            album: None,
            format: None,
            source: None,
            cover_art_url: None,
            size_bytes: None,
            duration_seconds: None,
            year: None,
            detail: None,
            track_count: None,
            lossless_count: None,
            failed_count: None,
            kept_count: None,
            upgrading_count: None,
            requested_by: None,
        }
    }
}
