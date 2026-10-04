//! Port of `Services/LastFm/LastFmService.cs`: Last.fm's web service as the radio and the search
//! bar read it. The records are `octo_core::last_fm::last_fm_service`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::json::element::{
    ElementResult, enumerate_array, get_double, get_int64, get_property, get_string, raw_text,
    try_get_property,
};
use octo_core::last_fm::{SimilarArtist, SimilarTrack, TrackInfo};
use octo_core::metadata::accept_language_header;
use octo_core::settings::{LastFmSettings, SettingsStore};
use parking_lot::Mutex;
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use super::net_parse::{parse_double, parse_long};
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;
use crate::services::http_client_factory::DEFAULT_TIMEOUT;

/// What the radio cache holds: the C# kept `object` and checked the type on the way out.
#[derive(Clone)]
enum RadioValue {
    Artists(Vec<SimilarArtist>),
    Tags(Vec<String>),
    Tracks(Vec<SimilarTrack>),
    Info(Option<TrackInfo>),
}

/// A value [`LastFmService::cached`] can keep: `cached.Value is T typed`.
trait Cacheable: Sized + Clone {
    fn wrap(self) -> RadioValue;
    fn unwrap(value: &RadioValue) -> Option<Self>;
}

impl Cacheable for Vec<SimilarArtist> {
    fn wrap(self) -> RadioValue {
        RadioValue::Artists(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Artists(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for Vec<String> {
    fn wrap(self) -> RadioValue {
        RadioValue::Tags(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Tags(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for Vec<SimilarTrack> {
    fn wrap(self) -> RadioValue {
        RadioValue::Tracks(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        match value {
            RadioValue::Tracks(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Cacheable for Option<TrackInfo> {
    fn wrap(self) -> RadioValue {
        RadioValue::Info(self)
    }
    fn unwrap(value: &RadioValue) -> Option<Self> {
        // A null TrackInfo was stored, but `null is TrackInfo?` is false: it never hits.
        match value {
            RadioValue::Info(Some(info)) => Some(Some(info.clone())),
            _ => None,
        }
    }
}

pub struct LastFmService {
    http: reqwest::Client,
    base_url: String,
    /// `IOptionsMonitor<LastFmSettings>`: read at every use.
    settings: Arc<SettingsStore>,

    // Behind locks because radio requests arrive on request tasks and this is written on
    // every miss.
    cache: Mutex<HashMap<String, (DateTime<Utc>, Vec<SimilarTrack>)>>,
    radio_cache: Mutex<HashMap<String, (DateTime<Utc>, RadioValue)>>,
    provider_gate: Semaphore,
}

impl LastFmService {
    pub const BASE_URL: &'static str = "https://ws.audioscrobbler.com/2.0/";

    /// The C# methods' default limits.
    pub const SIMILAR_TRACKS_LIMIT: i32 = 50;
    pub const SEARCH_LIMIT: i32 = 30;
    pub const ARTIST_TOP_TRACKS_LIMIT: i32 = 30;
    pub const SIMILAR_ARTISTS_LIMIT: i32 = 20;
    pub const TOP_TAGS_LIMIT: i32 = 10;
    pub const TAG_TOP_TRACKS_LIMIT: i32 = 50;

    /// The production client (`AddHttpClient<LastFmService>()`: the default client, 100 s).
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        Self::with_base_url(settings, Self::BASE_URL)
    }

    /// A client that asks another host: a test's mock server.
    pub fn with_base_url(settings: Arc<SettingsStore>, base_url: &str) -> Self {
        // IOptions<MetadataSettings>: the Accept-Language is applied once, at construction,
        // deliberately, as the C# applied it to its client.
        let mut headers = HeaderMap::new();
        if let Some(language) = accept_language_header::header_value(&settings.current().metadata)
            && let Ok(value) = HeaderValue::from_str(&language)
        {
            headers.insert(ACCEPT_LANGUAGE, value);
        }
        let http = client_builder()
            .timeout(DEFAULT_TIMEOUT)
            .default_headers(headers)
            .build()
            .expect("the Last.fm client builds");
        Self {
            http,
            base_url: base_url.to_string(),
            settings,
            cache: Mutex::new(HashMap::new()),
            radio_cache: Mutex::new(HashMap::new()),
            provider_gate: Semaphore::new(4),
        }
    }

    fn last_fm(&self) -> LastFmSettings {
        self.settings.current().last_fm.clone()
    }

    fn cache_expiry(&self) -> DateTime<Utc> {
        Utc::now() + TimeDelta::hours(i64::from(self.last_fm().effective_radio_cache_duration_hours()))
    }

    async fn get(&self, url: &str) -> anyhow::Result<HttpAnswer> {
        Ok(HttpAnswer::read(self.http.get(url).send().await?).await?)
    }

    /// `EnsureSuccessStatusCode`, then the body as JSON.
    async fn get_json(&self, url: &str) -> anyhow::Result<Value> {
        let answer = self.get(url).await?;
        ensure_success(&answer)?;
        Ok(answer.json()?)
    }

    pub async fn get_similar_tracks(&self, artist: &str, title: &str, limit: i32) -> Vec<SimilarTrack> {
        let cache_key = dotnet::to_lower_invariant(&format!("{artist}|{title}"));

        // Check cache
        let cached = self
            .cache
            .lock()
            .get(&cache_key)
            .filter(|(expiry, _)| *expiry > Utc::now())
            .map(|(_, tracks)| tracks.clone());
        if let Some(tracks) = cached {
            debug!(
                "Returning {} cached similar tracks for {artist} - {title}",
                tracks.len()
            );
            return take(tracks, limit);
        }

        match self
            .similar_tracks_uncached(artist, title, limit, &cache_key)
            .await
        {
            Ok(tracks) => take(tracks, limit),
            Err(e) => {
                error!(error = %e, "Error fetching similar tracks from Last.fm for {artist} - {title}");
                Vec::new()
            }
        }
    }

    async fn similar_tracks_uncached(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
        cache_key: &str,
    ) -> anyhow::Result<Vec<SimilarTrack>> {
        info!("Fetching similar tracks from Last.fm for {artist} - {title}");

        // Last.fm knows a song under one spelling. "$uicideboy$ - $UICIDE" or a title
        // carrying "(feat. X)" or "- Remastered 2011" can come back empty where the same song
        // written plainly does not, so those are asked before falling back to similar artists.
        // A renamed artist is filed under one name only: Last.fm has "Kanye West - Crack
        // Music" and nothing under "Ye", so the name the artist is best known by comes last.
        let known_name = SongIdentity::known_name(artist);
        let mut queries: Vec<_> = SongIdentity::query_variants(title, artist)
            .into_iter()
            .filter(|variant| !variant.artist.is_empty())
            .take(3)
            .collect();
        if let Some(known) = &known_name {
            queries.extend(
                SongIdentity::query_variants(title, known)
                    .into_iter()
                    .filter(|variant| !variant.artist.is_empty())
                    .take(2),
            );
        }

        let mut tracks = Vec::new();
        for variant in &queries {
            tracks = self
                .fetch_similar_tracks(&variant.artist, &variant.title, limit)
                .await?;
            if !tracks.is_empty() {
                break;
            }
        }

        info!("Found {} similar tracks from Last.fm", tracks.len());

        // Cache results
        self.cache
            .lock()
            .insert(cache_key.to_string(), (self.cache_expiry(), tracks.clone()));

        // If no similar tracks found, try getting top tracks from similar artists. For a
        // renamed artist that guess runs under the name Last.fm files them by: asked for
        // artists like "Ye", it answered for someone else, and a mix for "Ye - Crack Music"
        // came back as thirty Jon Anderson songs.
        if tracks.is_empty() {
            let guess_from = known_name.as_deref().unwrap_or(artist);
            info!("No similar tracks found, trying similar artists for {guess_from}");
            tracks = self.top_tracks_from_similar_artists(guess_from, limit).await;
        }

        Ok(tracks)
    }

    /// One track.getsimilar request, read into tracks.
    async fn fetch_similar_tracks(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
    ) -> anyhow::Result<Vec<SimilarTrack>> {
        let url = format!(
            "{}?method=track.getsimilar&artist={}&track={}&api_key={}&format=json&limit={limit}",
            self.base_url,
            dotnet::escape_data_string(artist),
            dotnet::escape_data_string(title),
            self.last_fm().api_key
        );
        let doc = self.get_json(&url).await?;
        Ok(read_similar_tracks(&doc)?)
    }

    async fn top_tracks_from_similar_artists(&self, artist: &str, limit: i32) -> Vec<SimilarTrack> {
        match self.top_tracks_from_similar_artists_inner(artist, limit).await {
            Ok(tracks) => tracks,
            Err(e) => {
                error!(error = %e, "Error fetching top tracks from similar artists for {artist}");
                Vec::new()
            }
        }
    }

    async fn top_tracks_from_similar_artists_inner(
        &self,
        artist: &str,
        limit: i32,
    ) -> anyhow::Result<Vec<SimilarTrack>> {
        // Get similar artists
        let artist_url = format!(
            "{}?method=artist.getsimilar&artist={}&api_key={}&format=json&limit=10",
            self.base_url,
            dotnet::escape_data_string(artist),
            self.last_fm().api_key
        );
        let artist_doc = self.get_json(&artist_url).await?;

        let mut similar_artists = Vec::new();
        if let Some(similar) = try_get_property(&artist_doc, "similarartists")?
            && let Some(array) = try_get_property(similar, "artist")?
        {
            for a in enumerate_array(array)?.iter().take(5) {
                if let Some(name) = get_string(get_property(a, "name")?)?.filter(|n| !n.is_empty()) {
                    similar_artists.push(name.to_string());
                }
            }
        }

        info!("Found {} similar artists for {artist}", similar_artists.len());

        // Get top tracks from each similar artist
        let mut tracks = Vec::new();
        for similar_artist in &similar_artists {
            let top_tracks_url = format!(
                "{}?method=artist.gettoptracks&artist={}&api_key={}&format=json&limit=10",
                self.base_url,
                dotnet::escape_data_string(similar_artist),
                self.last_fm().api_key
            );
            let top = self.get(&top_tracks_url).await?;
            if !top.is_success() {
                continue;
            }
            let top_doc = top.json()?;
            if let Some(top_tracks) = try_get_property(&top_doc, "toptracks")?
                && let Some(array) = try_get_property(top_tracks, "track")?
            {
                for track in enumerate_array(array)?.iter().take(10) {
                    let track_name = get_string(get_property(track, "name")?)?.unwrap_or("");
                    if !track_name.is_empty() {
                        tracks.push(SimilarTrack::new(similar_artist.as_str(), track_name, 0.5));
                    }
                }
            }
        }

        info!("Found {} top tracks from similar artists", tracks.len());

        Ok(take(tracks, limit))
    }

    /// Last.fm can answer at all. Search discovery needs only this: an API key.
    pub fn has_api_key(&self) -> bool {
        !self.settings.current().last_fm.api_key.is_empty()
    }

    /// Radio specifically is available. EnableRadio is a switch for the radio feature, so
    /// it belongs here and not on [`has_api_key`](Self::has_api_key): the two used to be one
    /// property, which meant turning radio off also silently emptied the search bar of
    /// discovery results, a setting doing something its name does not say.
    pub fn is_radio_enabled(&self) -> bool {
        self.has_api_key() && self.settings.current().last_fm.enable_radio
    }

    /// Free-form track search. Used by Search3 hijack so the search bar
    /// returns Last.fm-driven discovery results instead of just local hits.
    /// Last.fm's track.search is a fuzzy match: "drake" returns Drake tracks,
    /// "drake hotline" returns "Hotline Bling" first, etc.
    pub async fn search_tracks(&self, query: &str, limit: i32) -> Vec<SimilarTrack> {
        if dotnet::is_blank(query) {
            return Vec::new();
        }
        let url = format!(
            "{}?method=track.search&track={}&api_key={}&format=json&limit={limit}",
            self.base_url,
            dotnet::escape_data_string(query),
            self.last_fm().api_key
        );
        let result: anyhow::Result<Vec<SimilarTrack>> = async {
            let doc = self.get_json(&url).await?;
            Ok(read_search_tracks(&doc)?)
        }
        .await;
        match result {
            Ok(tracks) => {
                info!("Last.fm track.search '{query}' -> {} tracks", tracks.len());
                tracks
            }
            Err(e) => {
                warn!(error = %e, "Last.fm track.search failed for '{query}'");
                Vec::new()
            }
        }
    }

    /// Top tracks for a known artist. Used to pad a search when track.search
    /// returns thin results (e.g. one-word artist queries) and as the primary
    /// data source for "play this artist" radio behaviors.
    pub async fn get_artist_top_tracks(&self, artist: &str, limit: i32) -> Vec<SimilarTrack> {
        if dotnet::is_blank(artist) {
            return Vec::new();
        }
        let url = format!(
            "{}?method=artist.gettoptracks&artist={}&api_key={}&format=json&limit={limit}",
            self.base_url,
            dotnet::escape_data_string(artist),
            self.last_fm().api_key
        );
        let result: anyhow::Result<Vec<SimilarTrack>> = async {
            let doc = self.get_json(&url).await?;
            let mut tracks = Vec::new();
            if let Some(top) = try_get_property(&doc, "toptracks")?
                && let Some(array @ Value::Array(_)) = try_get_property(top, "track")?
            {
                for t in enumerate_array(array)? {
                    let name = match try_get_property(t, "name")? {
                        Some(n) => get_string(n)?,
                        None => None,
                    };
                    if let Some(name) = name.filter(|n| !n.is_empty()) {
                        tracks.push(SimilarTrack::new(artist, name, 1.0));
                    }
                }
            }
            Ok(tracks)
        }
        .await;
        result.unwrap_or_else(|e| {
            warn!(error = %e, "Last.fm artist.gettoptracks failed for '{artist}'");
            Vec::new()
        })
    }

    /// `Err` where the C# let an exception out of the method: an element of Last.fm's answer
    /// that is not the kind the reader needs (an artist that is not an object, say).
    pub async fn get_similar_artists(&self, artist: &str, limit: i32) -> ElementResult<Vec<SimilarArtist>> {
        self.cached(&format!("artist-similar|{artist}|{limit}"), || async {
            let doc = self
                .get_document(
                    "artist.getsimilar",
                    &[("artist", artist), ("limit", &limit.to_string())],
                )
                .await;
            let Some(doc) = doc else { return Ok(Vec::new()) };
            let Some(values) = try_array(&doc, "similarartists", "artist")? else {
                return Ok(Vec::new());
            };
            take_valid(values, limit, |item| {
                let name = text(item, "name")?;
                let similar = SimilarArtist {
                    r#match: number(item, "match")?,
                    name,
                };
                Ok((!similar.name.is_empty()).then_some(similar))
            })
        })
        .await
    }

    pub async fn get_artist_top_tags(&self, artist: &str, limit: i32) -> ElementResult<Vec<String>> {
        self.get_tags(
            "artist.gettoptags",
            &[("artist", artist)],
            &format!("artist-tags|{artist}|{limit}"),
            limit,
        )
        .await
    }

    pub async fn get_track_top_tags(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
    ) -> ElementResult<Vec<String>> {
        self.get_tags(
            "track.gettoptags",
            &[("artist", artist), ("track", title)],
            &format!("track-tags|{artist}|{title}|{limit}"),
            limit,
        )
        .await
    }

    pub async fn get_tag_top_tracks(&self, tag: &str, limit: i32) -> ElementResult<Vec<SimilarTrack>> {
        self.cached(&format!("tag-tracks|{tag}|{limit}"), || async {
            let doc = self
                .get_document("tag.gettoptracks", &[("tag", tag), ("limit", &limit.to_string())])
                .await;
            let Some(doc) = doc else { return Ok(Vec::new()) };
            let Some(values) = try_array(&doc, "tracks", "track")? else {
                return Ok(Vec::new());
            };
            take_valid(values, limit, |item| {
                let track = SimilarTrack::new(text_in(item, "artist", "name")?, text(item, "name")?, 1.0)
                    .with_duration(duration_seconds(item)?);
                Ok((!track.artist.is_empty() && !track.title.is_empty()).then_some(track))
            })
        })
        .await
    }

    pub async fn get_track_info(&self, artist: &str, title: &str) -> ElementResult<Option<TrackInfo>> {
        self.cached(&format!("track-info|{artist}|{title}"), || async {
            let doc = self
                .get_document("track.getInfo", &[("artist", artist), ("track", title)])
                .await;
            let Some(doc) = doc else { return Ok(None) };
            let Some(track) = try_get_property(&doc, "track")? else {
                return Ok(None);
            };
            let mut tags = Vec::new();
            if let Some(values) = try_array(track, "toptags", "tag")? {
                for item in values {
                    let name = text(item, "name")?;
                    if !name.is_empty() {
                        tags.push(name);
                    }
                }
            }
            let album = text_in(track, "album", "title")?;
            Ok(Some(TrackInfo {
                artist: text_in(track, "artist", "name")?,
                title: text(track, "name")?,
                album: (!album.is_empty()).then_some(album),
                duration: duration_seconds(track)?,
                tags,
            }))
        })
        .await
    }

    async fn get_tags(
        &self,
        method: &str,
        parameters: &[(&str, &str)],
        cache_key: &str,
        limit: i32,
    ) -> ElementResult<Vec<String>> {
        self.cached(cache_key, || async {
            let Some(doc) = self.get_document(method, parameters).await else {
                return Ok(Vec::new());
            };
            let Some(values) = try_array(&doc, "toptags", "tag")? else {
                return Ok(Vec::new());
            };
            take_valid(values, limit, |item| {
                let name = text(item, "name")?;
                Ok((!name.is_empty()).then_some(name))
            })
        })
        .await
    }

    async fn cached<T, F, Fut>(&self, key: &str, load: F) -> ElementResult<T>
    where
        T: Cacheable,
        F: FnOnce() -> Fut,
        Fut: Future<Output = ElementResult<T>>,
    {
        let key = dotnet::to_lower_invariant(key);
        let hit = self
            .radio_cache
            .lock()
            .get(&key)
            .filter(|(expiry, _)| *expiry > Utc::now())
            .and_then(|(_, value)| T::unwrap(value));
        if let Some(typed) = hit {
            return Ok(typed);
        }
        let value = load().await?;
        self.radio_cache
            .lock()
            .insert(key, (self.cache_expiry(), value.clone().wrap()));
        Ok(value)
    }

    async fn get_document(&self, method: &str, parameters: &[(&str, &str)]) -> Option<Value> {
        if !self.has_api_key() {
            return None;
        }
        let _permit = self
            .provider_gate
            .acquire()
            .await
            .expect("the gate is never closed");
        let api_key = self.last_fm().api_key;
        // new Dictionary(parameters) with method, api_key and format set after them.
        let mut query: Vec<(&str, &str)> = Vec::new();
        for (name, value) in parameters.iter().copied().chain([
            ("method", method),
            ("api_key", api_key.as_str()),
            ("format", "json"),
        ]) {
            match query.iter_mut().find(|(existing, _)| *existing == name) {
                Some(pair) => pair.1 = value,
                None => query.push((name, value)),
            }
        }
        let url = format!(
            "{}?{}",
            self.base_url,
            query
                .iter()
                .map(|(name, value)| format!(
                    "{}={}",
                    dotnet::escape_data_string(name),
                    dotnet::escape_data_string(value)
                ))
                .collect::<Vec<_>>()
                .join("&")
        );
        let answer = match self.get(&url).await {
            Ok(answer) => answer,
            Err(e) => {
                warn!(error = %e, "Last.fm {method} failed");
                return None;
            }
        };
        if answer.status.as_u16() == 429 {
            warn!("Last.fm rate limited {method}");
            return None;
        }
        if !answer.is_success() {
            return None;
        }
        match answer.json() {
            Ok(doc) => Some(doc),
            Err(e) => {
                warn!(error = %e, "Last.fm {method} failed");
                None
            }
        }
    }
}

/// `EnsureSuccessStatusCode`, with .NET's message.
fn ensure_success(answer: &HttpAnswer) -> anyhow::Result<()> {
    if answer.is_success() {
        return Ok(());
    }
    anyhow::bail!(
        "Response status code does not indicate success: {} ({}).",
        answer.status.as_u16(),
        answer.status.canonical_reason().unwrap_or("")
    )
}

/// `list.Take(limit)`.
fn take<T>(mut items: Vec<T>, limit: i32) -> Vec<T> {
    items.truncate(usize::try_from(limit).unwrap_or(0));
    items
}

/// `values.Select(read).Where(kept).Take(limit)`, evaluated lazily as LINQ did: an element after
/// the last one taken is never read, so it cannot fail the call.
fn take_valid<T>(
    values: &[Value],
    limit: i32,
    mut read: impl FnMut(&Value) -> ElementResult<Option<T>>,
) -> ElementResult<Vec<T>> {
    let limit = usize::try_from(limit).unwrap_or(0);
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    for value in values {
        if let Some(item) = read(value)? {
            out.push(item);
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

/// track.getsimilar's answer, read as `FetchSimilarTracksAsync` read it.
fn read_similar_tracks(doc: &Value) -> ElementResult<Vec<SimilarTrack>> {
    let mut tracks = Vec::new();
    let Some(similar_tracks) = try_get_property(doc, "similartracks")? else {
        return Ok(tracks);
    };
    let Some(track_array) = try_get_property(similar_tracks, "track")? else {
        return Ok(tracks);
    };
    for track in enumerate_array(track_array)? {
        let track_name = get_string(get_property(track, "name")?)?.unwrap_or("");
        let mut artist_name = "";
        if let Some(artist_obj) = try_get_property(track, "artist")? {
            artist_name = match try_get_property(artist_obj, "name")? {
                Some(name) => get_string(name)?.unwrap_or(""),
                None => "",
            };
        }

        let mut r#match = 0.0;
        if let Some(match_prop) = try_get_property(track, "match")? {
            // Last.fm returns match as a number, not a string
            match match_prop {
                Value::Number(_) => r#match = get_double(match_prop)?,
                Value::String(text) => r#match = parse_double(text, true).unwrap_or(0.0),
                _ => {}
            }
        }

        // Last.fm returns duration in milliseconds (sometimes a string,
        // sometimes a number, sometimes "0" when unknown; treat 0 as null
        // so we fall back to the placeholder default downstream).
        let mut duration_sec = None;
        if let Some(dur_el) = try_get_property(track, "duration")? {
            let dur_ms = match dur_el {
                Value::Number(_) => get_int64(dur_el)?,
                Value::String(text) => parse_long(text).unwrap_or(0),
                _ => 0,
            };
            if dur_ms > 1000 {
                // (int)(durMs / 1000): an unchecked narrowing.
                duration_sec = Some((dur_ms / 1000) as i32);
            }
        }

        if !track_name.is_empty() && !artist_name.is_empty() {
            tracks.push(SimilarTrack::new(artist_name, track_name, r#match).with_duration(duration_sec));
        }
    }
    Ok(tracks)
}

/// track.search's answer, read as `SearchTracksAsync` read it.
fn read_search_tracks(doc: &Value) -> ElementResult<Vec<SimilarTrack>> {
    let mut tracks = Vec::new();
    if let Some(results) = try_get_property(doc, "results")?
        && let Some(matches) = try_get_property(results, "trackmatches")?
        && let Some(track_array @ Value::Array(_)) = try_get_property(matches, "track")?
    {
        for t in enumerate_array(track_array)? {
            let name = match try_get_property(t, "name")? {
                Some(n) => get_string(n)?,
                None => None,
            };
            let artist = match try_get_property(t, "artist")? {
                Some(a) => get_string(a)?,
                None => None,
            };
            let listeners = match try_get_property(t, "listeners")? {
                Some(Value::String(text)) => parse_long(text),
                Some(other) => parse_long(&raw_text(other)),
                None => None,
            };
            if let (Some(name), Some(artist)) = (name, artist)
                && !name.is_empty()
                && !artist.is_empty()
            {
                tracks.push(SimilarTrack::new(artist, name, 1.0).with_listeners(listeners));
            }
        }
    }
    Ok(tracks)
}

fn try_array<'a>(root: &'a Value, container: &str, array: &str) -> ElementResult<Option<&'a [Value]>> {
    let Some(parent) = try_get_property(root, container)? else {
        return Ok(None);
    };
    match try_get_property(parent, array)? {
        Some(Value::Array(values)) => Ok(Some(values)),
        _ => Ok(None),
    }
}

/// `JsonElement.ToString()`: a string's text, a number's raw text, `True`/`False`, nothing for
/// null, and the JSON of an object or array.
fn element_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => text.clone(),
        other => raw_text(other),
    }
}

fn text(element: &Value, property: &str) -> ElementResult<String> {
    Ok(try_get_property(element, property)?
        .map(element_text)
        .unwrap_or_default())
}

fn text_in(element: &Value, parent: &str, property: &str) -> ElementResult<String> {
    match try_get_property(element, parent)? {
        Some(value @ Value::Object(_)) => text(value, property),
        _ => Ok(String::new()),
    }
}

fn number(element: &Value, property: &str) -> ElementResult<f64> {
    Ok(try_get_property(element, property)?
        .and_then(|value| parse_double(&element_text(value), false))
        .unwrap_or(0.0))
}

fn duration_seconds(element: &Value) -> ElementResult<Option<i32>> {
    let Some(value) = try_get_property(element, "duration")? else {
        return Ok(None);
    };
    match parse_long(&element_text(value)) {
        Some(milliseconds) if milliseconds > 0 => Ok(Some(if milliseconds > 1000 {
            (milliseconds / 1000) as i32
        } else {
            milliseconds as i32
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
#[path = "last_fm_service_tests.rs"]
mod tests;

/// The song-length chain's Last.fm step (`SoulseekMetadataService.CompleteLengthAsync`).
///
/// An answer the radio readers refuse (`Err`) counts as no length. In the C# that exception
/// escaped the length lookup and skipped its last step, the yt-dlp probe; here the probe is still
/// tried. Recorded in known-diffs.md.
#[async_trait::async_trait]
impl crate::services::soulseek::soulseek_metadata_service::LastFmTrackLengths for LastFmService {
    fn has_api_key(&self) -> bool {
        LastFmService::has_api_key(self)
    }

    async fn track_duration(&self, artist: &str, title: &str) -> Option<i32> {
        self.get_track_info(artist, title)
            .await
            .ok()
            .flatten()
            .and_then(|info| info.duration)
    }
}
