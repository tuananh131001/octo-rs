//! Port of `Services/Lidarr/LidarrClient.cs`. Its records and the JSON readers are
//! `octo_core::lidarr::lidarr_client`.
//!
//! The C# methods took a `CancellationToken` that every caller but the track fetcher left at
//! its default; here a caller that must stop early drops the future, and the track fetcher
//! races its token against each call.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet;
use octo_core::lidarr::lidarr_client::{
    child, int_or_zero, map_options, nullable_bool, nullable_int, nullable_long, nullable_str, object,
    parse_album, parse_track_number, select_best_album, str_or_empty, value_str,
};
use octo_core::lidarr::{
    LidarrAlbumCandidate, LidarrAlbumImportState, LidarrAlbumState, LidarrError, LidarrImportedTrack,
    LidarrOptions, LidarrSearchStarted,
};
use octo_core::settings::{LidarrSettings, SettingsStore};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::services::http_client_factory;

type JsonObject = Map<String, Value>;

/// Small, purpose-built client for the Lidarr v1 endpoints Octo needs.
pub struct LidarrClient {
    http: reqwest::Client,
    /// `IOptionsMonitor<LidarrSettings>`: the address and key are read at every request.
    settings: Arc<SettingsStore>,
}

impl LidarrClient {
    /// `client.Timeout = TimeSpan.FromSeconds(20)` on the default client.
    pub const TIMEOUT: Duration = Duration::from_secs(20);

    pub fn new(settings: Arc<SettingsStore>) -> Self {
        // IHttpClientFactory.CreateClient(): the default client, with its timeout set to 20 s.
        let http = reqwest::Client::builder()
            .no_gzip()
            .no_deflate()
            .timeout(Self::TIMEOUT)
            .build()
            .expect("the Lidarr HTTP client builds");
        LidarrClient { http, settings }
    }

    pub async fn is_reachable(&self) -> bool {
        match self.request(Method::GET, "/ping", None) {
            Ok(request) => self.send(request).await.is_ok(),
            Err(_) => false,
        }
    }

    /// Checks an address and key that are not saved yet, and answers the choices that server
    /// offers.
    pub async fn test_connection(&self, base_url: &str, api_key: &str) -> Result<LidarrOptions, LidarrError> {
        if dotnet::is_blank(base_url) || dotnet::is_blank(api_key) {
            return Err(LidarrError::InvalidOperation(
                "Lidarr URL and API key are required.".into(),
            ));
        }
        self.send(Self::request_to(
            Method::GET,
            "/api/v1/system/status",
            base_url,
            api_key,
            None,
        ))
        .await?;

        let (roots, quality, metadata) = tokio::join!(
            self.get_array_from("/api/v1/rootfolder", base_url, api_key),
            self.get_array_from("/api/v1/qualityprofile", base_url, api_key),
            self.get_array_from("/api/v1/metadataprofile", base_url, api_key),
        );
        map_options(&roots?, &quality?, &metadata?)
    }

    pub async fn get_options(&self) -> Result<LidarrOptions, LidarrError> {
        let (roots, quality, metadata) = tokio::join!(
            self.get_array("/api/v1/rootfolder"),
            self.get_array("/api/v1/qualityprofile"),
            self.get_array("/api/v1/metadataprofile"),
        );
        map_options(&roots?, &quality?, &metadata?)
    }

    pub async fn resolve_album(
        &self,
        artist: &str,
        album: &str,
        year: Option<i32>,
    ) -> Result<LidarrAlbumCandidate, LidarrError> {
        let term = dotnet::escape_data_string(&format!("{artist} {album}"));
        let rows = self
            .get_array(&format!("/api/v1/album/lookup?term={term}"))
            .await?;
        let candidates = rows
            .iter()
            .map(parse_album)
            .filter(|x| x.as_ref().map_or(true, |c| !c.foreign_album_id.is_empty()))
            .collect::<Result<Vec<_>, _>>()?;
        select_best_album(&candidates, artist, album, year)
            .cloned()
            .ok_or_else(|| {
                LidarrError::InvalidOperation(format!(
                    "Lidarr could not unambiguously match '{artist} - {album}'."
                ))
            })
    }

    /// The candidate for a MusicBrainz release group id, or None when Lidarr does not know it.
    pub async fn resolve_album_by_foreign_id(
        &self,
        foreign_album_id: &str,
    ) -> Result<Option<LidarrAlbumCandidate>, LidarrError> {
        let term = dotnet::escape_data_string(&format!("lidarr:{foreign_album_id}"));
        let rows = self
            .get_array(&format!("/api/v1/album/lookup?term={term}"))
            .await?;
        // FirstOrDefault: rows are read only until the first match.
        for row in &rows {
            let candidate = parse_album(row)?;
            if dotnet::eq_ignore_case(&candidate.foreign_album_id, foreign_album_id) {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    pub async fn ensure_album_and_search(
        &self,
        candidate: &LidarrAlbumCandidate,
    ) -> Result<i32, LidarrError> {
        Ok(self.start_album_search(candidate).await?.album_id)
    }

    /// The album as Lidarr has it now, or None when Lidarr does not have it yet.
    pub async fn find_album(&self, foreign_album_id: &str) -> Result<Option<LidarrAlbumState>, LidarrError> {
        let existing = self
            .get_array(&format!(
                "/api/v1/album?foreignAlbumId={}",
                dotnet::escape_data_string(foreign_album_id)
            ))
            .await?;
        match existing.first() {
            None => Ok(None),
            Some(first) => Ok(Some(LidarrAlbumState {
                id: int_or_zero(first, "id")?,
                monitored: nullable_bool(first, "monitored")?.unwrap_or(false),
            })),
        }
    }

    /// Monitor or unmonitor albums, leaving everything else about them alone.
    pub async fn set_albums_monitored(&self, album_ids: &[i32], monitored: bool) -> Result<(), LidarrError> {
        if album_ids.is_empty() {
            return Ok(());
        }
        let body = json!({ "albumIds": album_ids, "monitored": monitored });
        let request = self.request(Method::PUT, "/api/v1/album/monitor", Some(&body))?;
        self.send(request).await?;
        Ok(())
    }

    /// Delete one track file, from Lidarr and from disk (into Lidarr's recycle bin when it has one).
    pub async fn delete_track_file(&self, track_file_id: i32) -> Result<(), LidarrError> {
        let request = self.request(
            Method::DELETE,
            &format!("/api/v1/trackfile/{track_file_id}"),
            None,
        )?;
        self.send(request).await?;
        Ok(())
    }

    /// Adds the album when Lidarr lacks it, monitors it, and starts an AlbumSearch. Says whether the
    /// album was there before and monitored, so a caller that only borrowed it can put it back.
    pub async fn start_album_search(
        &self,
        candidate: &LidarrAlbumCandidate,
    ) -> Result<LidarrSearchStarted, LidarrError> {
        let settings = self.require_settings(true)?;
        let existing = self
            .get_array(&format!(
                "/api/v1/album?foreignAlbumId={}",
                dotnet::escape_data_string(&candidate.foreign_album_id)
            ))
            .await?;

        let album_id;
        let mut was_monitored = false;
        if let Some(first) = existing.first() {
            let mut resource = first.clone();
            album_id = int_or_zero(&resource, "id")?;
            was_monitored = nullable_bool(&resource, "monitored")?.unwrap_or(false);
            if !was_monitored {
                resource.insert("monitored".into(), Value::Bool(true));
                self.send_json(Method::PUT, &format!("/api/v1/album/{album_id}"), &resource)
                    .await?;
            }
            self.ensure_artist_monitored(object(&resource, "artist")).await?;
        } else {
            let mut resource = candidate.resource.clone();
            resource.shift_remove("id");
            resource.insert("monitored".into(), Value::Bool(true));
            resource.insert("addOptions".into(), json!({ "searchForNewAlbum": false }));

            let artist = object(&resource, "artist").ok_or_else(|| {
                LidarrError::InvalidOperation("Lidarr album lookup returned no artist resource.".into())
            })?;
            let foreign_artist_id = match nullable_str(artist, "foreignArtistId")? {
                Some(id) => Some(id),
                None => nullable_str(artist, "mbId")?,
            };
            let existing_artists = self.get_array("/api/v1/artist").await?;
            let mut existing_artist = None;
            if let Some(wanted) = foreign_artist_id.as_deref().filter(|id| !id.is_empty()) {
                for a in &existing_artists {
                    let id = match nullable_str(a, "foreignArtistId")? {
                        Some(id) => Some(id),
                        None => nullable_str(a, "mbId")?,
                    };
                    if id.as_deref().is_some_and(|id| dotnet::eq_ignore_case(id, wanted)) {
                        existing_artist = Some(a);
                        break;
                    }
                }
            }
            if let Some(existing_artist) = existing_artist {
                resource.insert(
                    "artistId".into(),
                    Value::from(int_or_zero(existing_artist, "id")?),
                );
                resource.insert("artist".into(), Value::Object(existing_artist.clone()));
            } else if let Some(Value::Object(artist)) = resource.get_mut("artist") {
                artist.shift_remove("id");
                artist.insert(
                    "rootFolderPath".into(),
                    settings
                        .root_folder_path
                        .clone()
                        .map_or(Value::Null, Value::String),
                );
                artist.insert(
                    "qualityProfileId".into(),
                    Value::from(settings.quality_profile_id),
                );
                artist.insert(
                    "metadataProfileId".into(),
                    Value::from(settings.metadata_profile_id),
                );
                artist.insert("monitored".into(), Value::Bool(true));
                artist.insert("monitorNewItems".into(), Value::from("none"));
                artist.insert("tags".into(), json!([]));
                // "none" would unmonitor the artist and, after its first refresh, this album too.
                artist.insert(
                    "addOptions".into(),
                    json!({
                        "monitor": "unknown",
                        "albumsToMonitor": [candidate.foreign_album_id],
                        "searchForMissingAlbums": false,
                    }),
                );
            }

            let added = self.send_json(Method::POST, "/api/v1/album", &resource).await?;
            album_id = int_or_zero(&added, "id")?;
            self.ensure_artist_monitored(existing_artist).await?;
        }

        self.send_json(
            Method::POST,
            "/api/v1/command",
            json!({ "name": "AlbumSearch", "albumIds": [album_id] })
                .as_object()
                .expect("an object literal"),
        )
        .await?;
        Ok(LidarrSearchStarted {
            album_id,
            existed: !existing.is_empty(),
            was_monitored,
        })
    }

    /// Lidarr neither upgrades nor re-searches albums of an unmonitored artist.
    async fn ensure_artist_monitored(&self, artist: Option<&JsonObject>) -> Result<(), LidarrError> {
        let Some(artist) = artist else {
            return Ok(());
        };
        if nullable_bool(artist, "monitored")? != Some(false) {
            return Ok(());
        }
        let id = int_or_zero(artist, "id")?;
        let mut full = self.get_object(&format!("/api/v1/artist/{id}")).await?;
        full.insert("monitored".into(), Value::Bool(true));
        self.send_json(Method::PUT, &format!("/api/v1/artist/{id}"), &full)
            .await?;
        Ok(())
    }

    pub async fn get_album_tracks(&self, album_id: i32) -> Result<Vec<LidarrImportedTrack>, LidarrError> {
        // Lidarr deliberately omits the nested trackFile resource from album track
        // list responses, so load the album's files separately and join by id.
        let tracks_path = format!("/api/v1/track?albumId={album_id}");
        let files_path = format!("/api/v1/trackFile?albumId={album_id}");
        let (tracks, files) = tokio::join!(self.get_array(&tracks_path), self.get_array(&files_path));
        let tracks = tracks?;
        let files = files?;
        let mut by_id: std::collections::HashMap<i32, &JsonObject> = std::collections::HashMap::new();
        for file in &files {
            let id = int_or_zero(file, "id")?;
            if by_id.insert(id, file).is_some() {
                return Err(LidarrError::Argument(format!(
                    "An item with the same key has already been added. Key: {id}"
                )));
            }
        }

        tracks
            .iter()
            .map(|row| {
                let track_file_id = nullable_int(row, "trackFileId")?.unwrap_or(0);
                let file = by_id.get(&track_file_id).copied();
                let artist = object(row, "artist");
                let quality = match file.and_then(|f| f.get("quality")).filter(|v| !v.is_null()) {
                    None => None,
                    Some(q) => match child(q, "quality")? {
                        None => None,
                        Some(inner) => child(inner, "name")?
                            .map(value_str)
                            .transpose()?
                            .map(str::to_string),
                    },
                };
                Ok(LidarrImportedTrack {
                    id: int_or_zero(row, "id")?,
                    title: str_or_empty(row, "title")?,
                    track_number: parse_track_number(&str_or_empty(row, "trackNumber")?),
                    duration_seconds: nullable_int(row, "duration")?.map(|ms| ms / 1000),
                    has_file: nullable_bool(row, "hasFile")?.unwrap_or(false),
                    path: match file {
                        Some(f) => nullable_str(f, "path")?,
                        None => None,
                    },
                    size_bytes: match file {
                        Some(f) => nullable_long(f, "size")?.unwrap_or(0),
                        None => 0,
                    },
                    artist: match artist {
                        Some(a) => nullable_str(a, "artistName")?,
                        None => None,
                    },
                    track_file_id: if file.is_some() { track_file_id } else { 0 },
                    quality,
                })
            })
            .collect()
    }

    pub async fn get_album_import_state(&self, album_id: i32) -> Result<LidarrAlbumImportState, LidarrError> {
        let album_path = format!("/api/v1/album/{album_id}");
        let (album, tracks) = tokio::join!(self.get_object(&album_path), self.get_album_tracks(album_id));
        let album = album?;
        let tracks = tracks?;
        let statistics = object(&album, "statistics");
        let (track_count, track_file_count) = match statistics {
            None => (
                tracks.len() as i32,
                tracks.iter().filter(|t| t.has_file).count() as i32,
            ),
            Some(s) => (int_or_zero(s, "trackCount")?, int_or_zero(s, "trackFileCount")?),
        };
        Ok(LidarrAlbumImportState {
            tracks,
            track_count,
            track_file_count,
        })
    }

    fn require_settings(&self, require_profiles: bool) -> Result<LidarrSettings, LidarrError> {
        let value = self.settings.current().lidarr.clone();
        if dotnet::is_null_or_white_space(value.base_url.as_deref())
            || dotnet::is_null_or_white_space(value.api_key.as_deref())
        {
            return Err(LidarrError::InvalidOperation(
                "Lidarr URL and API key are required.".into(),
            ));
        }
        if require_profiles
            && (dotnet::is_null_or_white_space(value.root_folder_path.as_deref())
                || value.quality_profile_id <= 0
                || value.metadata_profile_id <= 0)
        {
            return Err(LidarrError::InvalidOperation(
                "Choose a Lidarr root folder, quality profile, and metadata profile.".into(),
            ));
        }
        Ok(value)
    }

    /// `CreateRequest(method, path)`: against the saved address and key.
    fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Request, LidarrError> {
        let settings = self.require_settings(false)?;
        Ok(Self::request_to(
            method,
            path,
            settings.base_url.as_deref().unwrap_or_default(),
            settings.api_key.as_deref().unwrap_or_default(),
            body,
        ))
    }

    fn request_to(
        method: Method,
        path: &str,
        base_url: &str,
        api_key: &str,
        body: Option<&Value>,
    ) -> Request {
        Request {
            method,
            url: format!("{}{path}", base_url.trim_end_matches('/')),
            api_key: api_key.to_string(),
            // JsonContent.Create: the web serializer's escaping.
            body: body.map(octo_core::json::to_string),
        }
    }

    async fn send(&self, request: Request) -> Result<reqwest::Response, LidarrError> {
        let url = reqwest::Url::parse(&request.url).map_err(|_| {
            LidarrError::InvalidOperation(
                "An invalid request URI was provided. Either the request URI must be an absolute URI or BaseAddress must be set."
                    .into(),
            )
        })?;
        let mut builder = self.http.request(request.method, url);
        // TryAddWithoutValidation: a value a header cannot hold is left out.
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&request.api_key) {
            builder = builder.header("X-Api-Key", value);
        }
        if let Some(body) = request.body {
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
                .body(body);
        }
        let response = builder.send().await.map_err(transport_error)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.map_err(transport_error)?;
            return Err(LidarrError::Http(format!(
                "Lidarr returned HTTP {status}: {}",
                truncate(&body, 300)
            )));
        }
        Ok(response)
    }

    async fn read_json(&self, request: Request) -> Result<Value, LidarrError> {
        let response = self.send(request).await?;
        let text = response.text().await.map_err(transport_error)?;
        serde_json::from_str(&text).map_err(|e| LidarrError::Json(e.to_string()))
    }

    async fn get_array(&self, path: &str) -> Result<Vec<JsonObject>, LidarrError> {
        let request = self.request(Method::GET, path, None)?;
        Self::objects_of(self.read_json(request).await?)
    }

    async fn get_array_from(
        &self,
        path: &str,
        base_url: &str,
        api_key: &str,
    ) -> Result<Vec<JsonObject>, LidarrError> {
        let request = Self::request_to(Method::GET, path, base_url, api_key, None);
        Self::objects_of(self.read_json(request).await?)
    }

    /// `node.OfType<JsonObject>()`: the array's objects, anything else in it skipped.
    fn objects_of(node: Value) -> Result<Vec<JsonObject>, LidarrError> {
        match node {
            Value::Array(items) => Ok(items
                .into_iter()
                .filter_map(|item| match item {
                    Value::Object(o) => Some(o),
                    _ => None,
                })
                .collect()),
            _ => Err(LidarrError::InvalidOperation(
                "Lidarr returned an invalid array response.".into(),
            )),
        }
    }

    fn object_of(node: Value) -> Result<JsonObject, LidarrError> {
        match node {
            Value::Object(o) => Ok(o),
            _ => Err(LidarrError::InvalidOperation(
                "Lidarr returned an invalid object response.".into(),
            )),
        }
    }

    async fn get_object(&self, path: &str) -> Result<JsonObject, LidarrError> {
        let request = self.request(Method::GET, path, None)?;
        Self::object_of(self.read_json(request).await?)
    }

    async fn send_json(
        &self,
        method: Method,
        path: &str,
        body: &JsonObject,
    ) -> Result<JsonObject, LidarrError> {
        let body = Value::Object(body.clone());
        let request = self.request(method, path, Some(&body))?;
        Self::object_of(self.read_json(request).await?)
    }
}

/// One request, built before it is sent (`HttpRequestMessage`).
struct Request {
    method: Method,
    url: String,
    api_key: String,
    body: Option<String>,
}

/// What HttpClient threw for a request that got no answer: its timeout, or the connection.
fn transport_error(error: reqwest::Error) -> LidarrError {
    if error.is_timeout() {
        LidarrError::Canceled(http_client_factory::timeout_message(LidarrClient::TIMEOUT))
    } else {
        LidarrError::Http(http_client_factory::connect_failure_message(&error))
    }
}

/// `value[..max]` in UTF-16 code units, without cutting a character in two.
fn truncate(value: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in value.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &value[..i];
        }
    }
    value
}

#[cfg(test)]
#[path = "lidarr_client_tests.rs"]
mod tests;
