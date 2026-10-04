//! Port of `Services/Library/UpgradeQueue.cs`: only the job, as far as getUpgrades writes it.
//!
//! STUB(5-B): replaced when 5-B (queues and sweeps) lands with the queue, its states and its
//! state file.

use chrono::{DateTime, Utc};

/// One "better quality" job.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpgradeJob {
    pub navidrome_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub suffix: Option<String>,

    /// The weekly upgrade's key for this file (path and size), when the page knew it, so a
    /// song with no FLAC anywhere is not tried again by the weekly run for four weeks.
    pub attempt_key: Option<String>,

    /// Who it acts as: the person who asked in an app, or the admin signed in on the page.
    pub requested_by: String,
    pub origin: String,

    /// `UpgradeStates`: "queued", "waiting", "working", "upgraded", "notFound", "rehearsed",
    /// "skipped" or "failed".
    pub state: String,
    pub detail: Option<String>,

    /// provider:externalId of the replacement download, once it is queued.
    pub acquisition_key: Option<String>,
    pub queued_utc: DateTime<Utc>,
    pub updated_utc: DateTime<Utc>,

    /// When it started running, for how long it took.
    pub started_utc: Option<DateTime<Utc>>,
}
