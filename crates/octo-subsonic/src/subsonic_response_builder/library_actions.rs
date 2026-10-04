//! Port of `SubsonicResponseBuilder.LibraryActions.cs`, the parts that need only settings.
//! The answer for a `LibraryActionOutcome` and getUpgrades take app types and are in the
//! `octo` crate.

use octo_core::settings::library_action::{LibraryAction, LibraryActionSettings};
use serde_json::json;

use super::{SUBSONIC_VERSION, SubsonicReply, SubsonicResponseBuilder};

/// The OpenSubsonic extension a client checks for before it offers a library action.
pub const LIBRARY_ACTIONS_EXTENSION: &str = "octoLibraryActions";

/// Version 2 adds the upgrade action and getUpgrades; version 1 is still listed.
pub const LIBRARY_ACTIONS_EXTENSION_VERSION: i32 = 2;

/// The Delete action, named for what a person sees: the song leaves the library, and the file
/// waits in quarantine.
pub const REMOVE_ACTION: &str = "remove";

/// The Better quality action, from version 2: look for a higher quality copy and swap it in, the
/// song keeping its place. Queued, never run inside the request.
pub const UPGRADE_ACTION: &str = "upgrade";

impl SubsonicResponseBuilder {
    /// getLibraryActions: what this server lets the caller do. Always JSON. The field names are a
    /// contract with the Octo app.
    ///
    /// (C#'s defaults: `parallel` 1, `upgrade_ready` true, `upgrade_source` "Soulseek".)
    pub fn create_library_actions_response(
        &self,
        settings: &LibraryActionSettings,
        username: Option<&str>,
        parallel: i32,
        upgrade_ready: bool,
        upgrade_source: &str,
    ) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryActions": {
                "enabled": settings.enabled,
                "allowed": settings.is_allowed(username),
                "dryRun": settings.dry_run,
                "actions": offered_actions(settings, upgrade_ready),
                // 0 means kept until someone removes it by hand.
                "keepDays": settings.effective_quarantine_retention_days(),
                // How many upgrades run at once, which is how many downloads may.
                "parallel": parallel,
                // Where an upgrade looks, for a client to say so rather than assume. Null when upgrade is not offered.
                "upgradeSource": upgrade_ready.then_some(upgrade_source),
            },
        }))
    }

    /// libraryAction with a state of its own, such as "queued" for an upgrade. Always ok, so
    /// the client reads the state rather than an error, even when nothing was done.
    pub fn create_library_action_response(
        &self,
        song_id: &str,
        state: &str,
        detail: Option<&str>,
        action: &str,
    ) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryAction": {
                "id": song_id,
                "action": action,
                "state": state,
                "detail": detail,
            },
        }))
    }
}

/// The actions offered, by the names the app knows them by.
fn offered_actions(settings: &LibraryActionSettings, upgrade_ready: bool) -> Vec<&'static str> {
    let enabled: Vec<LibraryAction> = settings
        .effective_actions()
        .into_iter()
        .filter(|action| action.enabled)
        .map(|action| action.action)
        .collect();
    let mut offered = Vec::new();
    if enabled.contains(&LibraryAction::Delete) {
        offered.push(REMOVE_ACTION);
    }
    if enabled.contains(&LibraryAction::BetterQuality) && upgrade_ready {
        offered.push(UPGRADE_ACTION);
    }
    offered
}
