//! Port of `Services/Library/LibraryActionPlaylistWorker.cs`: the worker, and the playlist rows
//! and parsers `NavidromePlaylistApi` returns.

use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::settings::{
    LibraryAction, LibraryActionDefinition, LibraryActionSettings, NoticeKind, SettingsStore,
};
use serde_json::Value;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::library_action_executor::{LibraryActionExecutor, LibraryActionRequest};
use super::library_action_journal::action_name;
use super::{LibraryActionJournal, LibraryActionQuarantine, NavidromePlaylistApi, NavidromeSongPathResolver};
use crate::services::subsonic::NavidromeIdentityService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRow {
    pub id: String,
    pub name: String,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTrackRow {
    pub position: String,
    pub media_file_id: String,
}

/// Watches the action playlists and applies what people put in them.
///
/// A worker rather than a request hook, because applying an action can involve a download and
/// nothing that removes a file should run inside a request.
pub struct LibraryActionPlaylistWorker {
    executor: Arc<LibraryActionExecutor>,
    journal: Arc<LibraryActionJournal>,
    quarantine: Arc<LibraryActionQuarantine>,
    resolver: Arc<NavidromeSongPathResolver>,
    identity: NavidromeIdentityService,
    api: Arc<NavidromePlaylistApi>,
    /// `IOptionsMonitor<LibraryActionSettings>`. `Enabled`, `PlaylistsEnabled` and the poll
    /// interval are decided once, when the worker starts; the sweep reads the rest live.
    settings: Arc<SettingsStore>,
}

impl LibraryActionPlaylistWorker {
    pub fn new(
        executor: Arc<LibraryActionExecutor>,
        journal: Arc<LibraryActionJournal>,
        quarantine: Arc<LibraryActionQuarantine>,
        resolver: Arc<NavidromeSongPathResolver>,
        identity: NavidromeIdentityService,
        api: Arc<NavidromePlaylistApi>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        LibraryActionPlaylistWorker {
            executor,
            journal,
            quarantine,
            resolver,
            identity,
            api,
            settings,
        }
    }

    /// `ExecuteAsync`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        // Deliberately captured: the switches and the interval are decided when the worker starts.
        let settings = self.settings.current().library_actions.clone();

        // Feature-gated by early return, which leaves the service registered but idle. Turning
        // file removal on is a deliberate act, so it is not worth a restart-free toggle.
        if !settings.enabled || !settings.playlists_enabled {
            info!("Library actions are off; the playlist worker is idle");
            return Ok(());
        }

        if !self.identity.has_admin_identity() {
            // One clear line at startup rather than one silent failure per action.
            warn!(
                "Library actions are enabled but Octo has no Navidrome admin credential. Set Subsonic:AdminUsername and AdminPassword, or sign in through Octo once as a Navidrome admin. Until then no action can find its file, so none will run."
            );
            return Ok(());
        }

        // Decide what half-finished actions meant before doing anything new.
        let quarantine = self.quarantine.clone();
        self.journal
            .reconcile(Some(&move |path: &str| quarantine.restore(path).moved));

        // PeriodicTimer: the first tick one interval in, and missed ticks coalesced into one.
        let period = settings.effective_poll_interval();
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        while !stopping.is_cancelled() {
            self.sweep(&stopping).await;

            tokio::select! {
                _ = timer.tick() => {}
                _ = stopping.cancelled() => break,
            }
        }
        Ok(())
    }

    /// One pass over the action playlists. Public so a test or the dashboard can run one.
    pub async fn sweep(&self, stopping: &CancellationToken) {
        let settings = self.settings.current().library_actions.clone();
        if !settings.enabled || !settings.playlists_enabled {
            return;
        }

        let wanted = Self::wanted_playlists(&settings);
        if wanted.is_empty() {
            return;
        }

        let mut budget = settings.effective_max_actions_per_cycle();

        for playlist in self.api.list_playlists().await {
            if budget <= 0 || stopping.is_cancelled() {
                break;
            }
            let Some(action) = find(&wanted, &playlist.name) else {
                continue;
            };

            // The owner comes from Navidrome's own record, not from a request parameter. That
            // is the strongest form of the allowlist check available.
            if !settings.is_allowed(Some(&playlist.owner)) {
                debug!(
                    "Skipping '{}': {} is not on the allowlist",
                    playlist.name, playlist.owner
                );
                continue;
            }

            let applied = self
                .apply_playlist(&playlist, action.action, budget, stopping)
                .await;
            budget -= applied;
        }

        let music_root = self.resolver.music_root();
        let quarantine = self.quarantine.clone();
        let _ = tokio::task::spawn_blocking(move || quarantine.sweep(&music_root)).await;
    }

    async fn apply_playlist(
        &self,
        playlist: &PlaylistRow,
        action: LibraryAction,
        budget: i32,
        stopping: &CancellationToken,
    ) -> i32 {
        let tracks = self.api.list_tracks(&playlist.id).await;
        if tracks.is_empty() {
            return 0;
        }

        let mut consumed: Vec<String> = Vec::new();
        let mut applied = 0;

        for track in tracks.iter().take(budget.max(0) as usize) {
            if stopping.is_cancelled() {
                break;
            }

            // Per-item handling is mandatory: one failed action must not stop the sweep.
            match self
                .executor
                .apply(LibraryActionRequest::new(
                    action,
                    track.media_file_id.clone(),
                    playlist.owner.clone(),
                ))
                .await
            {
                Ok(outcome) => {
                    info!(
                        "Library action {} for {} by {}: {} - {}",
                        action_name(action),
                        track.media_file_id,
                        playlist.owner,
                        outcome.state.name(),
                        outcome.detail.as_deref().unwrap_or("")
                    );

                    if outcome.consumed() {
                        consumed.push(track.media_file_id.clone());
                    }
                    applied += 1;
                }
                Err(e) => {
                    error!(
                        "Library action {} threw for {}: {e:#}",
                        action_name(action),
                        track.media_file_id
                    );
                }
            }
        }

        if !consumed.is_empty() {
            self.remove_tracks(&playlist.id, &consumed).await;
        }
        applied
    }

    /// Remove the tracks whose action landed.
    ///
    /// PlaylistTrack.ID is the 1-based POSITION, reassigned on every mutation, so the list is
    /// re-read immediately before deleting and positions are mapped from the media file ids that
    /// were actually applied. A track the user removed in the meantime is simply not there and
    /// is skipped rather than deleting whatever now sits at its old position. The delete is one
    /// bulk call because two sequential single deletes renumber between them.
    async fn remove_tracks(&self, playlist_id: &str, media_file_ids: &[String]) {
        let current = self.api.list_tracks(playlist_id).await;
        let positions: Vec<String> = current
            .into_iter()
            .filter(|track| media_file_ids.contains(&track.media_file_id))
            .map(|track| track.position)
            .filter(|position| !position.is_empty())
            .collect();
        if positions.is_empty() {
            return;
        }

        match self.api.remove_positions(playlist_id, &positions).await {
            Ok(true) => {}
            Ok(false) => warn!(
                "Could not clear {} applied track(s) from playlist {playlist_id}. They stay put; the journal stops the action running twice.",
                positions.len()
            ),
            Err(e) => warn!("Could not clear applied tracks from playlist {playlist_id}: {e}"),
        }
    }

    /// The playlists whose tracks are commands, by title (compared ignoring case).
    pub fn wanted_playlists(settings: &LibraryActionSettings) -> Vec<(String, LibraryActionDefinition)> {
        let mut wanted: Vec<(String, LibraryActionDefinition)> = settings
            .effective_actions()
            .into_iter()
            .filter(|action| action.enabled)
            .map(|action| (settings.playlist_title(&action), action))
            .collect();
        // A notice playlist named like an action playlist would have every track Octo asked about
        // acted on. Octo's own questions are never commands, whatever they are called.
        for kind in [NoticeKind::Review, NoticeKind::Duplicates] {
            let title = settings.notice_title(kind);
            wanted.retain(|(name, _)| !dotnet::eq_ignore_case(name, &title));
        }
        wanted
    }
}

/// The action a playlist title asks for, ignoring case.
pub fn find<'a>(
    wanted: &'a [(String, LibraryActionDefinition)],
    name: &str,
) -> Option<&'a LibraryActionDefinition> {
    wanted
        .iter()
        .find(|(title, _)| dotnet::eq_ignore_case(title, name))
        .map(|(_, action)| action)
}

pub fn parse_playlists(root: &Value) -> Vec<PlaylistRow> {
    let Value::Array(items) = root else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for item in items {
        let (Some(id), Some(name)) = (text(item, "id"), text(item, "name")) else {
            continue;
        };
        if id.is_empty() || name.is_empty() {
            continue;
        }
        rows.push(PlaylistRow {
            id,
            name,
            owner: text(item, "ownerName").unwrap_or_default(),
        });
    }
    rows
}

pub fn parse_tracks(root: &Value) -> Vec<PlaylistTrackRow> {
    let Value::Array(items) = root else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for item in items {
        let Some(media_file_id) = text(item, "mediaFileId").filter(|m| !m.is_empty()) else {
            continue;
        };
        rows.push(PlaylistTrackRow {
            position: text(item, "id").unwrap_or_default(),
            media_file_id,
        });
    }
    rows
}

/// A string member, or a number's text; anything else is missing.
fn text(element: &Value, name: &str) -> Option<String> {
    match element.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "library_action_playlist_worker_tests.rs"]
mod tests;
