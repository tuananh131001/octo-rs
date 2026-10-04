//! `Octo.Models.Settings.ListenBrainzSettings` (the `ListenBrainz` section).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::text::eq_ignore_case;

/// Listen submission for tracks that never touch Navidrome's library. Plays of local
/// tracks are scrobbled by Navidrome itself (Octo relays every scrobble unchanged);
/// plays of external tracks, previews from search and Continuous Radio, would
/// otherwise be lost to the listener's history. Each listener authorises with their
/// own ListenBrainz user token; a single default token covers a one-person install.
/// Every value is read through IOptionsMonitor at submit time.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ListenBrainzSettings {
    /// Default user token, used for any Navidrome user without an entry in
    /// `user_tokens`. From listenbrainz.org/settings. Empty disables
    /// submission for users without their own token.
    /// Environment variable: LISTENBRAINZ__TOKEN
    pub token: String,

    /// Navidrome username to ListenBrainz user token, for installs with more than one
    /// listener. Environment variable form: LISTENBRAINZ__USERTOKENS__alice=...
    ///
    /// The C# dictionary compares keys with OrdinalIgnoreCase; look entries up with
    /// [`ListenBrainzSettings::user_token`], not `get`.
    pub user_tokens: IndexMap<String, String>,

    /// Submit a listen when an external track completes (a scrobble for an
    /// external id, or a Continuous Radio track played to the end).
    pub submit_external_plays: bool,
}

impl Default for ListenBrainzSettings {
    fn default() -> Self {
        Self {
            token: String::new(),
            user_tokens: IndexMap::new(),
            submit_external_plays: true,
        }
    }
}

impl ListenBrainzSettings {
    /// `UserTokens.TryGetValue(name)` with the dictionary's OrdinalIgnoreCase comparer.
    pub fn user_token(&self, username: &str) -> Option<&str> {
        self.user_tokens
            .get(username)
            .or_else(|| {
                self.user_tokens
                    .iter()
                    .find(|(k, _)| eq_ignore_case(k, username))
                    .map(|(_, v)| v)
            })
            .map(String::as_str)
    }

    /// The token that applies to this listener, or None when none is configured.
    pub fn token_for(&self, username: &str) -> Option<String> {
        if !self.submit_external_plays {
            return None;
        }
        if !username.trim().is_empty()
            && let Some(own) = self.user_token(username.trim())
            && !own.trim().is_empty()
        {
            return Some(own.trim().to_string());
        }
        if self.token.trim().is_empty() {
            None
        } else {
            Some(self.token.trim().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // LastFmRadioCoreTests.ListenBrainzSettings_PickTheListenersTokenThenTheDefaultAndRespectTheSwitch
    #[test]
    fn listen_brainz_settings_pick_the_listeners_token_then_the_default_and_respect_the_switch() {
        let mut settings = ListenBrainzSettings {
            token: " default ".into(),
            user_tokens: [
                ("Bob".to_string(), "bobs".to_string()),
                ("carol".into(), "  ".into()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(settings.token_for("bob").as_deref(), Some("bobs"));
        assert_eq!(settings.token_for("carol").as_deref(), Some("default"));
        assert_eq!(settings.token_for("alice").as_deref(), Some("default"));
        assert_eq!(settings.token_for("").as_deref(), Some("default"));
        settings.token = String::new();
        assert_eq!(settings.token_for("alice"), None);
        assert_eq!(settings.token_for("bob").as_deref(), Some("bobs"));
        settings.submit_external_plays = false;
        assert_eq!(settings.token_for("bob"), None);
    }
}
