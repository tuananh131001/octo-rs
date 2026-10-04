//! Port of the parts of `Services/Library/NoticeQueue.cs` that library actions (5-A) call: a
//! Keep answers the question Octo asked, and an action that ran answers every question about
//! that track.
//!
//! STUB(5-B): replaced when 5-B (queues and sweeps) lands with the whole queue: its file
//! (`notice-queue.json`), the Duplicates entries, lookups, submissions and the bound. What is here
//! is ported as the C# wrote it, in memory.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet;
use octo_core::fingerprint::verification::VerificationResult;
use octo_core::models::domain::Song;
use octo_core::settings::NoticeKind;
use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoticeState {
    #[default]
    Waiting,
    Queued,
    Kept,
    Acted,
    Dismissed,
    Expired,
}

/// One question Octo asked a person (the fields library actions read).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NoticeEntry {
    pub key: String,
    pub kind: NoticeKind,
    pub username: String,
    pub local_path: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub navidrome_id: Option<String>,
    pub group_key: Option<String>,
    pub state: NoticeState,
    pub fingerprint: Option<String>,
    pub duration_seconds: i32,
    pub queued_utc: Option<DateTime<Utc>>,
    pub resolved_utc: Option<DateTime<Utc>>,
}

impl NoticeEntry {
    pub fn is_open(&self) -> bool {
        matches!(self.state, NoticeState::Waiting | NoticeState::Queued)
    }
}

/// The questions Octo asks through playlists (Review, Duplicates).
#[derive(Default)]
pub struct NoticeQueue {
    entries: Mutex<HashMap<String, NoticeEntry>>,
}

/// Where a kind sorts (`OrderBy(entry => entry.Kind)`): Review first.
fn kind_order(kind: NoticeKind) -> u8 {
    match kind {
        NoticeKind::Review => 0,
        NoticeKind::Duplicates => 1,
    }
}

fn same_user(a: &str, b: &str) -> bool {
    dotnet::eq_ignore_case(a, b.trim())
}

impl NoticeQueue {
    /// A person who removes a track from Review and then drops it into Delete answered with
    /// Delete, even though the sweep saw the removal first.
    const ACTED_AFTER_DISMISS_WINDOW: TimeDelta = TimeDelta::minutes(10);

    pub fn new() -> Self {
        Self::default()
    }

    pub fn review_key(username: &str, local_path: &str) -> String {
        format!(
            "review|{}|{local_path}",
            dotnet::to_lower_invariant(username.trim())
        )
    }

    /// Ask `username` about a download verification could not settle. A file already asked
    /// about is never asked about again, in any state.
    pub fn add_review(
        &self,
        username: &str,
        local_path: &str,
        song: &Song,
        verdict: &VerificationResult,
    ) -> bool {
        let key = Self::review_key(username, local_path);
        let mut entries = self.entries.lock();
        if entries.contains_key(&key) {
            return false;
        }
        entries.insert(
            key.clone(),
            NoticeEntry {
                key,
                kind: NoticeKind::Review,
                username: username.trim().to_string(),
                local_path: local_path.to_string(),
                artist: song.artist.clone(),
                title: song.title.clone(),
                album: Some(song.album.clone()),
                state: NoticeState::Waiting,
                fingerprint: verdict.fingerprint.clone(),
                duration_seconds: verdict.duration_seconds,
                ..Default::default()
            },
        );
        true
    }

    pub fn for_user(&self, username: &str, kind: NoticeKind) -> Vec<NoticeEntry> {
        self.entries
            .lock()
            .values()
            .filter(|entry| entry.kind == kind && same_user(&entry.username, username))
            .cloned()
            .collect()
    }

    pub fn set_navidrome_id(&self, key: &str, navidrome_id: &str) {
        if let Some(entry) = self.entries.lock().get_mut(key) {
            entry.navidrome_id = Some(navidrome_id.to_string());
        }
    }

    pub fn mark_queued(&self, keys: &[String]) {
        let mut entries = self.entries.lock();
        for key in keys {
            if let Some(entry) = entries.get_mut(key) {
                entry.state = NoticeState::Queued;
                entry.queued_utc = Some(Utc::now());
            }
        }
    }

    /// A person kept this track, which answers every question Octo had open for them about it.
    /// For Review the question was about the file, so it is answered for everyone asked. For
    /// Duplicates, keeping one copy says the copies are on purpose, so that person's whole group
    /// is settled. A rating or the Keep playlist cannot say which playlist it came from, so a
    /// track in both is answered in both. Returns the entry Octo had open for this person, Review
    /// first, or None when Octo never asked them about this track.
    pub fn mark_kept(&self, username: &str, navidrome_id: &str) -> Option<NoticeEntry> {
        let mut entries = self.entries.lock();
        let mut mine: Vec<NoticeEntry> = entries
            .values()
            .filter(|entry| {
                entry.is_open()
                    && entry.navidrome_id.as_deref() == Some(navidrome_id)
                    && same_user(&entry.username, username)
            })
            .cloned()
            .collect();
        if mine.is_empty() {
            return None;
        }
        mine.sort_by_key(|entry| kind_order(entry.kind));

        let now = Utc::now();
        for asked in &mine {
            let open: Vec<String> = entries
                .values()
                .filter(|entry| entry.is_open() && entry.kind == asked.kind)
                .map(|entry| entry.key.clone())
                .collect();
            for key in open {
                let Some(entry) = entries.get_mut(&key) else {
                    continue;
                };
                let answered = if asked.kind == NoticeKind::Review {
                    entry.navidrome_id.as_deref() == Some(navidrome_id)
                        || entry.local_path == asked.local_path
                } else {
                    entry.group_key == asked.group_key
                        && dotnet::eq_ignore_case(&entry.username, &asked.username)
                };
                if !answered {
                    continue;
                }
                entry.state = if asked.kind == NoticeKind::Review {
                    NoticeState::Kept
                } else {
                    NoticeState::Dismissed
                };
                entry.resolved_utc = Some(now);
                // One answer is one submission. A download nobody requested is asked of every
                // allowed user, and each of their entries carries the same fingerprint.
                if entry.key != asked.key {
                    entry.fingerprint = None;
                }
            }
        }
        Some(mine.swap_remove(0))
    }

    /// An action ran on this track, so every open question about it is answered, for everyone:
    /// the file it was about has gone or changed. The rest of a duplicate group it belonged to
    /// is no longer the group Octo asked about, so those questions expire, and the next scan
    /// asks again if copies remain.
    pub fn mark_acted(&self, navidrome_id: &str) {
        let now = Utc::now();
        let mut entries = self.entries.lock();
        let mut groups: Vec<String> = Vec::new();
        for entry in entries.values_mut() {
            if entry.navidrome_id.as_deref() != Some(navidrome_id) {
                continue;
            }
            let recently_dismissed = entry.state == NoticeState::Dismissed
                && entry
                    .resolved_utc
                    .is_some_and(|at| now - at < Self::ACTED_AFTER_DISMISS_WINDOW);
            if !entry.is_open() && !recently_dismissed {
                continue;
            }
            entry.state = NoticeState::Acted;
            entry.resolved_utc = Some(now);
            entry.fingerprint = None;
            if let Some(group) = &entry.group_key {
                groups.push(group.clone());
            }
        }
        for entry in entries.values_mut() {
            if entry.is_open() && entry.group_key.as_ref().is_some_and(|g| groups.contains(g)) {
                entry.state = NoticeState::Expired;
                entry.resolved_utc = Some(now);
            }
        }
    }
}
