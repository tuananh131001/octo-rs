//! Port of `Services/LastFm/LastFmRadioTrackResolver.cs`: resolves a Last.fm candidate locally
//! first, then as an external placeholder. Whether a library hit is the recommended recording
//! is `octo_core::last_fm::last_fm_radio_track_resolver::is_same_recording`.

use std::sync::Arc;

use indexmap::IndexMap;
use octo_core::json::element::{ElementResult, enumerate_array, get_int32, get_string, try_get_property};
use octo_core::models::domain::Song;
use octo_core::soulseek::soulseek_metadata_service::RoutingKind;
use serde_json::Value;
use tracing::debug;

pub use octo_core::last_fm::last_fm_radio_track_resolver::is_same_recording;

use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::subsonic::SubsonicProxyService;

#[derive(Clone)]
pub struct LastFmRadioTrackResolver {
    proxy: SubsonicProxyService,
    metadata: Arc<dyn IMusicMetadataService>,
    registry: Arc<ExternalIdRegistry>,
}

impl LastFmRadioTrackResolver {
    pub fn new(
        proxy: SubsonicProxyService,
        metadata: Arc<dyn IMusicMetadataService>,
        registry: Arc<ExternalIdRegistry>,
    ) -> Self {
        LastFmRadioTrackResolver {
            proxy,
            metadata,
            registry,
        }
    }

    /// The song a scrobbled id names: an external route as its placeholder, or the library song
    /// Navidrome describes. None when neither knows it.
    pub async fn resolve_scrobble(
        &self,
        id: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        if let Some(route) = self.registry.lookup(id)
            && route.snapshot().kind == RoutingKind::Song
        {
            let route = route.snapshot();
            return Some(Song {
                id: id.to_string(),
                artist: route.artist.unwrap_or_default(),
                title: route.title.unwrap_or_default(),
                album: route.album.unwrap_or_default(),
                duration: route.duration,
                is_local: false,
                external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                external_id: Some(id.to_string()),
                ..Default::default()
            });
        }
        let mut parameters = authenticated_parameters.clone();
        parameters.insert("id".into(), id.to_string());
        parameters.insert("f".into(), "json".into());
        let result = self.proxy.relay_safe("rest/getSong", &parameters).await?;
        if result.body.is_empty() {
            return None;
        }
        let read = || -> anyhow::Result<Option<Song>> {
            let document: Value = serde_json::from_slice(&result.body)?;
            let Some(response) = try_get_property(&document, "subsonic-response")? else {
                return Ok(None);
            };
            let Some(song) = try_get_property(response, "song")? else {
                return Ok(None);
            };
            Ok(Some(Song {
                id: id.to_string(),
                artist: string(song, "artist")?,
                title: string(song, "title")?,
                album: string(song, "album")?,
                genre: nullable_string(song, "genre")?,
                duration: integer(song, "duration")?,
                is_local: true,
                ..Default::default()
            }))
        };
        match read() {
            Ok(song) => song,
            Err(error) => {
                debug!("scrobble metadata lookup failed for {id}: {error}");
                None
            }
        }
    }

    /// The library's copy when it has the recording, else the first external placeholder.
    pub async fn resolve(
        &self,
        artist: &str,
        title: &str,
        duration: Option<i32>,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        if let Some(local) = self
            .try_find_local_match(artist, title, authenticated_parameters)
            .await
        {
            return Some(local);
        }
        self.metadata
            .search_songs_by_artist_title(artist, title, 1, duration)
            .await
            .into_iter()
            .next()
    }

    /// Navidrome's search for the artist and title, as the listener; the first hit that is the
    /// same recording.
    pub async fn try_find_local_match(
        &self,
        artist: &str,
        title: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        let mut parameters = authenticated_parameters.clone();
        parameters.insert("query".into(), format!("{artist} {title}"));
        parameters.insert("songCount".into(), "3".into());
        parameters.insert("albumCount".into(), "0".into());
        parameters.insert("artistCount".into(), "0".into());
        parameters.insert("f".into(), "json".into());

        let result = self.proxy.relay_safe("rest/search3", &parameters).await?;
        if result.body.is_empty() {
            return None;
        }
        let read = || -> anyhow::Result<Option<Song>> {
            let document: Value = serde_json::from_slice(&result.body)?;
            let Some(response) = try_get_property(&document, "subsonic-response")? else {
                return Ok(None);
            };
            let Some(search) = try_get_property(response, "searchResult3")? else {
                return Ok(None);
            };
            let Some(songs @ Value::Array(_)) = try_get_property(search, "song")? else {
                return Ok(None);
            };
            for song in enumerate_array(songs)? {
                let hit_artist = string(song, "artist")?;
                let hit_title = string(song, "title")?;
                let id = string(song, "id")?;
                if id.is_empty() || !is_same_recording(artist, title, &hit_artist, &hit_title) {
                    continue;
                }

                return Ok(Some(Song {
                    id,
                    title: hit_title,
                    artist: hit_artist,
                    artist_id: nullable_string(song, "artistId")?,
                    album: string(song, "album")?,
                    album_id: nullable_string(song, "albumId")?,
                    duration: integer(song, "duration")?,
                    year: integer(song, "year")?,
                    track: integer(song, "track")?,
                    genre: nullable_string(song, "genre")?,
                    suffix: nullable_string(song, "suffix")?,
                    bit_rate: integer(song, "bitRate")?,
                    isrcs: texts(song, "isrc")?,
                    is_local: true,
                    ..Default::default()
                }));
            }
            Ok(None)
        };
        match read() {
            Ok(song) => song,
            Err(error) => {
                debug!("local radio match lookup failed for {artist} - {title}: {error}");
                None
            }
        }
    }
}

/// `TryGetProperty(name) ? GetString() ?? "" : ""`.
fn string(element: &Value, name: &str) -> ElementResult<String> {
    Ok(nullable_string(element, name)?.unwrap_or_default())
}

fn nullable_string(element: &Value, name: &str) -> ElementResult<Option<String>> {
    match try_get_property(element, name)? {
        Some(value) => Ok(get_string(value)?.map(str::to_string)),
        None => Ok(None),
    }
}

/// A number's `GetInt32()`; anything else is null.
fn integer(element: &Value, name: &str) -> ElementResult<Option<i32>> {
    match try_get_property(element, name)? {
        Some(value @ Value::Number(_)) => Ok(Some(get_int32(value)?)),
        _ => Ok(None),
    }
}

/// A list of text as Navidrome sent it, OpenSubsonic's `isrc`; empty when absent.
fn texts(element: &Value, name: &str) -> ElementResult<Vec<String>> {
    Ok(match try_get_property(element, name)? {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::common::test_fakes::FakeMetadata;
    use crate::services::test_support::received;

    async fn resolver(body: &str, metadata: Arc<FakeMetadata>) -> (LastFmRadioTrackResolver, MockServer) {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let resolver = LastFmRadioTrackResolver::new(
            SubsonicProxyService::new(settings),
            metadata,
            Arc::new(ExternalIdRegistry::in_memory()),
        );
        (resolver, server)
    }

    fn auth(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // LastFmRadioTrackResolverTests.Resolver_PrefersAuthenticatedLocalMatchBeforeExternalPlaceholder
    #[tokio::test]
    async fn resolver_prefers_authenticated_local_match_before_external_placeholder() {
        let metadata = Arc::new(FakeMetadata::default());
        let (resolver, server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[{"id":"local-1","artist":"The Artist","title":"The Song","album":"Album","duration":200}]}}}"#,
            metadata.clone(),
        )
        .await;
        let song = resolver
            .resolve(
                "The Artist",
                "The Song",
                Some(200),
                &auth(&[("u", "alice"), ("t", "token"), ("s", "salt")]),
            )
            .await
            .expect("a song");
        assert!(song.is_local);
        assert_eq!(song.id, "local-1");
        let requests = received(&server).await;
        assert!(requests[0].url.query().unwrap().contains("u=alice"));
        // Strict: the metadata service was never asked (its hits table is empty, and nothing
        // came from it).
        assert_eq!(song.album, "Album");
    }

    // LastFmRadioTrackResolverTests.Resolver_UsesExternalPlaceholderOnlyAfterLocalMiss
    #[tokio::test]
    async fn resolver_uses_external_placeholder_only_after_local_miss() {
        let metadata = Arc::new(FakeMetadata::default());
        let expected = Song {
            id: "external-1".into(),
            artist: "A".into(),
            title: "T".into(),
            is_local: false,
            ..Default::default()
        };
        metadata
            .hits
            .lock()
            .insert(("A".into(), "T".into()), expected.clone());
        let (resolver, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[]}}}"#,
            metadata,
        )
        .await;
        let song = resolver
            .resolve("A", "T", Some(180), &auth(&[("u", "alice")]))
            .await;
        assert_eq!(song.map(|song| song.id), Some(expected.id));
    }

    /// Rust-only: a hit that is another recording is passed over, an answer that is not the
    /// shape read gives no match, and a scrobbled external id is its route.
    #[tokio::test]
    async fn other_recordings_and_odd_answers_are_no_match() {
        let (matching, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[{"id":"x","artist":"Airbourne","title":"Sexy Boy"},{"id":"y","artist":"Air","title":"Sexy Boy","duration":"long"}]}}}"#,
            Arc::new(FakeMetadata::default()),
        )
        .await;
        assert!(
            matching
                .try_find_local_match("Air", "Sexy Boy", &IndexMap::new())
                .await
                .is_some()
        );
        let (odd, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[{"id":"y","artist":"Air","title":"Sexy Boy","duration":200.5}]}}}"#,
            Arc::new(FakeMetadata::default()),
        )
        .await;
        assert!(
            odd.try_find_local_match("Air", "Sexy Boy", &IndexMap::new())
                .await
                .is_none()
        );

        let id =
            matching
                .registry
                .register(octo_core::soulseek::soulseek_metadata_service::SoulseekRouting {
                    artist: Some("A".into()),
                    title: Some("T".into()),
                    ..Default::default()
                });
        let song = matching.resolve_scrobble(&id, &IndexMap::new()).await.unwrap();
        assert!(!song.is_local);
        assert_eq!(song.external_provider.as_deref(), Some("soulseek"));
        let song = matching.resolve_scrobble("navidrome-id", &IndexMap::new()).await;
        assert!(song.is_none());
    }
}
