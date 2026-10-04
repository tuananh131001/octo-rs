//! Port of `Services/Library/LibraryActionPlaylistProvisioner.cs`.
//!
//! The C# registered it scoped (`AddScoped`), because `SubsonicProxyService` is: one per
//! request, over that request's proxy. Its once-per-boot set was therefore once per request in
//! practice. A caller builds one per request the same way (`new(request's proxy, settings)`).

use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::services::subsonic::SubsonicProxyService;

/// Makes sure an allowed user has a playlist for each enabled action.
///
/// Created PER USER with THEIR credentials, so the playlists are theirs and private. An
/// admin-owned public playlist would be visible and writable to every account on the server,
/// which would hand the delete button to people who are not on the allowlist. A private
/// playlist per user is exactly the right blast radius.
pub struct LibraryActionPlaylistProvisioner {
    /// Usernames already provisioned by this instance (keys compared ignoring case), so this runs
    /// once per user rather than on every playlist listing.
    done: Mutex<HashSet<String>>,
    proxy: SubsonicProxyService,
    /// `IOptionsMonitor<LibraryActionSettings>`: read at every call.
    settings: Arc<SettingsStore>,
}

impl LibraryActionPlaylistProvisioner {
    pub fn new(proxy: SubsonicProxyService, settings: Arc<SettingsStore>) -> Self {
        LibraryActionPlaylistProvisioner {
            done: Mutex::new(HashSet::new()),
            proxy,
            settings,
        }
    }

    /// Create whatever is missing.
    ///
    /// `existing_names` comes from the playlist listing the caller just relayed, so this costs
    /// no extra request in the common case where everything already exists.
    pub async fn ensure(
        &self,
        username: &str,
        existing_names: &[String],
        auth_parameters: &IndexMap<String, String>,
    ) {
        let settings = self.settings.current();
        let settings = &settings.library_actions;
        if !settings.enabled || !(settings.playlists_enabled || settings.notices_enabled()) {
            return;
        }
        if !settings.is_allowed(Some(username)) {
            return;
        }
        if !self.done.lock().insert(dotnet::ordinal_ignore_case_key(username)) {
            return;
        }

        // The notice playlists Octo fills are the user's own too, for the same reason: they are
        // private, and the admin identity fills them.
        let actions: Vec<String> = if settings.playlists_enabled {
            settings
                .effective_actions()
                .iter()
                .filter(|action| action.enabled)
                .map(|action| settings.playlist_title(action))
                .collect()
        } else {
            Vec::new()
        };
        let mut wanted: Vec<String> = Vec::new();
        for title in actions.into_iter().chain(
            settings
                .enabled_notice_kinds()
                .into_iter()
                .map(|kind| settings.notice_title(kind)),
        ) {
            let exists = existing_names
                .iter()
                .any(|name| dotnet::eq_ignore_case(name, &title));
            let repeated = wanted.iter().any(|name| dotnet::eq_ignore_case(name, &title));
            if !exists && !repeated {
                wanted.push(title);
            }
        }
        if wanted.is_empty() {
            return;
        }

        for title in wanted {
            // The user's own credentials, so the playlist belongs to them. Drop anything that
            // would make this look like an edit of an existing playlist. Keys compare ignoring
            // case, as the C# dictionary did.
            let mut parameters: IndexMap<String, String> = auth_parameters
                .iter()
                .filter(|(key, _)| {
                    !["id", "playlistId", "songId"]
                        .iter()
                        .any(|dropped| dotnet::eq_ignore_case(key, dropped))
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            match parameters
                .keys()
                .find(|key| dotnet::eq_ignore_case(key, "name"))
                .cloned()
            {
                Some(key) => {
                    parameters.insert(key, title.clone());
                }
                None => {
                    parameters.insert("name".into(), title.clone());
                }
            }

            // Never fails into the playlist listing: a failure here costs a playlist, not the
            // user's ability to see their own.
            if self
                .proxy
                .relay_safe("rest/createPlaylist", parameters.iter())
                .await
                .is_some()
            {
                info!("Created action playlist '{title}' for {username}");
            } else {
                warn!("Could not create action playlist '{title}' for {username}");
            }
        }
    }

    /// Forget the once-per-boot marker, so a settings change re-provisions.
    pub fn reset(&self) {
        self.done.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, LibraryActionSettings, SubsonicSettings};
    use wiremock::matchers::{any, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Rust-only (the C# had no test of its own): the missing titles are created once, with the
    /// user's own sign-in and never an id that would make it an edit.
    #[tokio::test]
    async fn the_missing_playlists_are_created_once_as_the_user() {
        let navidrome = MockServer::start().await;
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json"),
            )
            .mount(&navidrome)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&navidrome)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings);
        let auth: IndexMap<String, String> = [("u", "alice"), ("t", "tok"), ("s", "salt"), ("ID", "p9")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // Keep comes on with Review; the Review playlist itself exists already.
        provisioner.ensure("bob", &[], &auth).await;
        provisioner
            .ensure("Alice", &["▸ review".to_string()], &auth)
            .await;
        provisioner.ensure("alice", &[], &auth).await;

        let created: Vec<(String, String, bool)> = navidrome
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .map(|r| {
                let query: Vec<(String, String)> = r.url.query_pairs().into_owned().collect();
                let get = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
                (
                    get("name").unwrap_or_default(),
                    get("u").unwrap_or_default(),
                    query.iter().any(|(k, _)| k.eq_ignore_ascii_case("id")),
                )
            })
            .collect();
        assert_eq!(created, [("🛠 Keep".to_string(), "alice".to_string(), false)]);
    }
}
