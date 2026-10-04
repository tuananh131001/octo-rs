//! STUB(5-B): replaced when 5-B (queues and sweeps) lands with the port of
//! `Services/Library/UpgradeQueue.cs`. Only what `HeartOwnership` (4-D) needs is here: adding a
//! job per song not already queued, and reading the jobs back, in memory. No file, no cap, no
//! worker.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// A song to look for, with what the asker already knows about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeAsk {
    pub navidrome_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub suffix: Option<String>,
    pub attempt_key: Option<String>,
}

/// One song someone asked to find in higher quality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeJob {
    pub navidrome_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub suffix: Option<String>,
    pub attempt_key: Option<String>,
    /// Who it acts as: the person who asked in an app, or the admin signed in on the page.
    pub requested_by: String,
    pub origin: String,
    pub queued_utc: DateTime<Utc>,
}

#[derive(Default)]
pub struct UpgradeQueue {
    jobs: Mutex<Vec<UpgradeJob>>,
}

impl UpgradeQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one job per song not already queued. Answers each song's job as it now stands,
    /// and a reason for the songs left out (never, in this stub).
    pub fn add(
        &self,
        asks: Vec<UpgradeAsk>,
        requester: &str,
        origin: &str,
    ) -> (Vec<UpgradeJob>, Option<String>) {
        let mut jobs = self.jobs.lock();
        let mut answer = Vec::new();
        for ask in asks {
            if ask.navidrome_id.trim().is_empty() {
                continue;
            }
            if let Some(existing) = jobs.iter().find(|job| job.navidrome_id == ask.navidrome_id) {
                answer.push(existing.clone());
                continue;
            }
            let job = UpgradeJob {
                navidrome_id: ask.navidrome_id,
                title: ask.title,
                artist: ask.artist,
                album: ask.album,
                suffix: ask.suffix,
                attempt_key: ask.attempt_key,
                requested_by: requester.to_string(),
                origin: origin.to_string(),
                queued_utc: Utc::now(),
            };
            jobs.push(job.clone());
            answer.push(job);
        }
        (answer, None)
    }

    /// Every job, as copies, oldest first.
    pub fn snapshot(&self) -> Vec<UpgradeJob> {
        self.jobs.lock().clone()
    }
}
