//! Port of `Services/Library/NavidromePlaylistApi.cs`.

use std::sync::Arc;

use axum::http::StatusCode;
use octo_core::settings::SettingsStore;
use octo_subsonic::subsonic_request_parser::escape_data_string;
use reqwest::Method;
use serde_json::Value;
use tracing::warn;

use super::library_action_playlist_worker::{self as playlist_worker, PlaylistRow, PlaylistTrackRow};
use super::quality_upgrade_worker::{self, LibrarySongRow};
use crate::services::subsonic::NavidromeIdentityService;

/// Navidrome's native playlist API, as the admin. Shared by the action-playlist sweep, which
/// removes what people asked for, and the notice worker, which adds what Octo is asking about.
///
/// An admin may edit any user's playlist (Navidrome's checkWritable), which is what lets Octo
/// fill a playlist the user owns while the user keeps it private.
///
/// The C# methods took a `CancellationToken` that only cancelled the HTTP calls; here a caller
/// that stops drops the future instead.
pub struct NavidromePlaylistApi {
    http: reqwest::Client,
    identity: NavidromeIdentityService,
    settings: Arc<SettingsStore>,
}

/// A request to send with the admin token, built again for the retry.
struct Call {
    method: Method,
    url: String,
    json_body: Option<String>,
}

impl NavidromePlaylistApi {
    pub fn new(
        http: reqwest::Client,
        identity: NavidromeIdentityService,
        settings: Arc<SettingsStore>,
    ) -> Self {
        NavidromePlaylistApi {
            http,
            identity,
            settings,
        }
    }

    fn base_url(&self) -> String {
        self.settings
            .current()
            .subsonic
            .url
            .clone()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string()
    }

    pub async fn list_playlists(&self) -> Vec<PlaylistRow> {
        let call = Call::get(format!("{}/api/playlist?_start=0&_end=1000", self.base_url()));
        match self.read_json(call).await {
            Ok(Some(root)) => playlist_worker::parse_playlists(&root),
            Ok(None) => Vec::new(),
            Err(message) => {
                warn!("Could not list playlists: {message}");
                Vec::new()
            }
        }
    }

    pub async fn list_tracks(&self, playlist_id: &str) -> Vec<PlaylistTrackRow> {
        let call = Call::get(format!(
            "{}/api/playlist/{}/tracks?_start=0&_end=500",
            self.base_url(),
            escape_data_string(playlist_id)
        ));
        match self.read_json(call).await {
            Ok(Some(root)) => playlist_worker::parse_tracks(&root),
            Ok(None) => Vec::new(),
            Err(message) => {
                warn!("Could not list tracks in playlist {playlist_id}: {message}");
                Vec::new()
            }
        }
    }

    /// One page of Navidrome's own song list, with how many rows it held, or `None` when it
    /// could not be read. The native list rather than search3: only this one reports the real
    /// library path, and the weekly upgrade remembers files by path (#70).
    pub async fn list_songs(&self, start: i32, count: i32) -> Option<(Vec<LibrarySongRow>, usize)> {
        let call = Call::get(format!(
            "{}/api/song?_start={start}&_end={}&_sort=id&_order=ASC",
            self.base_url(),
            start + count
        ));
        // HttpClient's own timeout is Navidrome not answering, the same as any other failure.
        let parsed = match self.read_json(call).await {
            Ok(Some(root)) => quality_upgrade_worker::parse_songs(&root),
            Ok(None) => return None,
            Err(message) => Err(message),
        };
        match parsed {
            Ok(rows) => rows,
            Err(message) => {
                warn!("Could not list songs: {message}");
                None
            }
        }
    }

    /// Remove tracks by POSITION. Positions are reassigned on every change, so callers re-read
    /// the list immediately before and send every position in one call. `Err` when the call
    /// itself failed, which the C# let escape to its caller.
    pub async fn remove_positions(&self, playlist_id: &str, positions: &[String]) -> Result<bool, String> {
        if positions.is_empty() {
            return Ok(true);
        }
        let query = positions
            .iter()
            .map(|p| format!("id={}", escape_data_string(p)))
            .collect::<Vec<_>>()
            .join("&");
        let call = Call {
            method: Method::DELETE,
            url: format!(
                "{}/api/playlist/{}/tracks?{query}",
                self.base_url(),
                escape_data_string(playlist_id)
            ),
            json_body: None,
        };
        let status = self.try_send(call).await?.map(|r| r.status());
        if status.is_some_and(|s| s.is_success()) {
            return Ok(true);
        }
        warn!(
            "Could not remove {} track(s) from playlist {playlist_id}: HTTP {}",
            positions.len(),
            status_text(status)
        );
        Ok(false)
    }

    /// Append tracks. Navidrome answers {"added": n}. `Err` when the call itself failed.
    pub async fn add_tracks(&self, playlist_id: &str, media_file_ids: &[String]) -> Result<bool, String> {
        if media_file_ids.is_empty() {
            return Ok(true);
        }
        let call = Call {
            method: Method::POST,
            url: format!(
                "{}/api/playlist/{}/tracks",
                self.base_url(),
                escape_data_string(playlist_id)
            ),
            json_body: Some(octo_core::json::to_string(
                &serde_json::json!({ "ids": media_file_ids }),
            )),
        };
        let status = self.try_send(call).await?.map(|r| r.status());
        if status.is_some_and(|s| s.is_success()) {
            return Ok(true);
        }
        warn!(
            "Could not add {} track(s) to playlist {playlist_id}: HTTP {}",
            media_file_ids.len(),
            status_text(status)
        );
        Ok(false)
    }

    /// The JSON of a successful answer: `Ok(None)` when there is no answer or it is not a
    /// success, `Err` when the call or the parse failed (the C# caught those).
    async fn read_json(&self, call: Call) -> Result<Option<Value>, String> {
        let Some(response) = self.try_send(call).await? else {
            return Ok(None);
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        let body = response.bytes().await.map_err(|e| e.to_string())?;
        serde_json::from_slice(&body).map(Some).map_err(|e| e.to_string())
    }

    /// Send with the admin token, and on a 401 log in again and resend ONCE. `None` when there
    /// is no admin credential at all, or the fresh token is refused too.
    async fn try_send(&self, call: Call) -> Result<Option<reqwest::Response>, String> {
        if self
            .settings
            .current()
            .subsonic
            .url
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Ok(None);
        }
        let Some(jwt) = self.identity.ensure_admin_jwt().await.filter(|j| !j.is_empty()) else {
            return Ok(None);
        };

        let response = self
            .request(&call, &jwt)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(Some(response));
        }

        self.identity.invalidate_admin_jwt(&jwt);
        let fresh = self.identity.ensure_admin_jwt().await;
        let Some(fresh) = fresh.filter(|f| !f.is_empty() && *f != jwt) else {
            warn!("Navidrome refused Octo's admin token and no fresh one could be had");
            return Ok(None);
        };
        self.request(&call, &fresh)
            .send()
            .await
            .map(Some)
            .map_err(|e| e.to_string())
    }

    fn request(&self, call: &Call, jwt: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(call.method.clone(), &call.url)
            .header("X-Nd-Authorization", format!("Bearer {jwt}"));
        if let Some(body) = &call.json_body {
            request = request
                .header("Content-Type", "application/json; charset=utf-8")
                .body(body.clone());
        }
        request
    }
}

impl Call {
    fn get(url: String) -> Call {
        Call {
            method: Method::GET,
            url,
            json_body: None,
        }
    }
}

/// `(int?)response?.StatusCode` as a log value: empty for no answer.
fn status_text(status: Option<StatusCode>) -> String {
    status.map(|s| s.as_u16().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use octo_core::settings::{AppSettings, SubsonicSettings};
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(url: &str) -> NavidromePlaylistApi {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                url: Some(url.to_string()),
                admin_username: Some("admin".into()),
                admin_password: Some("pw".into()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let http = crate::services::http_client_factory::default_client();
        let identity = NavidromeIdentityService::new(Arc::clone(&settings), http.clone());
        NavidromePlaylistApi::new(http, identity, settings)
    }

    async fn login(server: &MockServer, tokens: &[&str]) {
        for token in tokens {
            Mock::given(method("POST"))
                .and(path("/auth/login"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "token": token, "isAdmin": true, "username": "admin",
                })))
                .up_to_n_times(1)
                .mount(server)
                .await;
        }
    }

    #[tokio::test]
    async fn a_refused_token_is_renewed_once_and_the_call_sent_again() {
        let server = MockServer::start().await;
        login(&server, &["old", "new"]).await;
        Mock::given(method("GET"))
            .and(path("/api/playlist"))
            .and(header("X-Nd-Authorization", "Bearer old"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/playlist"))
            .and(header("X-Nd-Authorization", "Bearer new"))
            .and(query_param("_end", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "p1", "name": "Keep", "ownerName": "alice" },
                { "id": "p2", "name": "" },
                { "id": 3, "name": "Numbered" },
            ])))
            .mount(&server)
            .await;

        let rows = api(&server.uri()).list_playlists().await;

        assert_eq!(
            rows,
            [
                PlaylistRow {
                    id: "p1".into(),
                    name: "Keep".into(),
                    owner: "alice".into()
                },
                PlaylistRow {
                    id: "3".into(),
                    name: "Numbered".into(),
                    owner: String::new()
                },
            ]
        );
    }

    #[tokio::test]
    async fn tracks_are_added_as_json_and_removed_by_position() {
        let server = MockServer::start().await;
        login(&server, &["jwt"]).await;
        Mock::given(method("POST"))
            .and(path("/api/playlist/pl%201/tracks"))
            .and(body_json(serde_json::json!({ "ids": ["a", "b"] })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "added": 2 })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/playlist/pl%201/tracks"))
            .and(query_param("id", "1"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let api = api(&server.uri());

        assert_eq!(api.add_tracks("pl 1", &["a".into(), "b".into()]).await, Ok(true));
        assert_eq!(
            api.remove_positions("pl 1", &["1".into(), "2".into()]).await,
            Ok(true)
        );
        assert_eq!(api.add_tracks("pl 1", &[]).await, Ok(true));
        let deleted = server
            .received_requests()
            .await
            .expect("recording")
            .into_iter()
            .find(|r| r.method == wiremock::http::Method::DELETE)
            .expect("a delete");
        assert_eq!(deleted.url.query(), Some("id=1&id=2"));
    }

    #[tokio::test]
    async fn songs_are_listed_a_page_at_a_time() {
        let server = MockServer::start().await;
        login(&server, &["jwt"]).await;
        Mock::given(method("GET"))
            .and(path("/api/song"))
            .and(query_param("_start", "100"))
            .and(query_param("_end", "150"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "s1", "path": "A/B/1.flac", "size": 10, "bitRate": 900, "duration": 2.5, "suffix": "flac" },
                { "id": "s2", "path": "A/B/2.flac", "missing": true },
            ])))
            .mount(&server)
            .await;

        let (rows, count) = api(&server.uri()).list_songs(100, 50).await.expect("a page");

        assert_eq!(count, 2);
        assert_eq!(rows.len(), 1);
        // Math.Round: half to even.
        assert_eq!(
            (rows[0].size, rows[0].bit_rate, rows[0].duration),
            (10, 900, Some(2))
        );
    }

    #[tokio::test]
    async fn no_admin_identity_means_no_call() {
        let api = NavidromePlaylistApi::new(
            crate::services::http_client_factory::default_client(),
            NavidromeIdentityService::new(
                Arc::new(SettingsStore::for_tests(AppSettings::default())),
                crate::services::http_client_factory::default_client(),
            ),
            Arc::new(SettingsStore::for_tests(AppSettings {
                subsonic: SubsonicSettings {
                    url: Some("http://127.0.0.1:1".into()),
                    ..Default::default()
                },
                ..Default::default()
            })),
        );
        assert!(api.list_playlists().await.is_empty());
        assert_eq!(api.add_tracks("p", &["a".into()]).await, Ok(false));
    }
}
