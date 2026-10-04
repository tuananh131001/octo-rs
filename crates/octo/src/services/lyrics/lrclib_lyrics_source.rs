//! Port of `Services/Lyrics/LrclibLyricsSource.cs`.

use std::time::Duration;

use async_trait::async_trait;
use octo_core::common::SongQuery;
use octo_core::common::dotnet::{eq_ignore_case, is_null_or_white_space};
use octo_core::lyrics::{
    ILyricsSource, LyricsCandidate, LyricsIdentity, LyricsLookup, LyricsQuery, LyricsResult, LyricsSearch,
    LyricsText, LyricsfileReader,
};
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::lyrics_http::{
    ShapeError, escape_data_string, get_i64, prop, retry_after_delta, round_to_int, str_prop, try_parse_long,
};

/// LRCLIB: open, keyless, and wide coverage of synced lyrics for current music. Its docs ask
/// callers to identify themselves, to go one request at a time with a short gap, and to honour
/// Retry-After; it sheds load with 503s often enough that all three matter. A few entries also
/// time their words (hasWordSync); those words live in the entry's Lyricsfile, not its LRC.
pub struct LrclibLyricsSource {
    client: reqwest::Client,
    base_url: String,
    /// The one-at-a-time gate, holding when the last request went out.
    gate: tokio::sync::Mutex<Option<Instant>>,
    cool_down_until: Mutex<Option<Instant>>,
}

/// One answer: the JSON, or none (a 404), and whether it was "not now".
struct Answer {
    json: Option<Value>,
    transient: bool,
}

impl LrclibLyricsSource {
    pub const CLIENT_NAME: &'static str = super::lyrics_http::LYRICS_CLIENT_NAME;
    pub const DEFAULT_BASE_URL: &'static str = "https://lrclib.net";
    const MINIMUM_GAP: Duration = Duration::from_millis(250);
    const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

    /// The source over the `lyrics` client ([`super::lyrics_http::lyrics_http_client`]).
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_base_url(client, Self::DEFAULT_BASE_URL)
    }

    /// The same against another address, for tests.
    pub fn with_base_url(client: reqwest::Client, base_url: &str) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            gate: tokio::sync::Mutex::new(None),
            cool_down_until: Mutex::new(None),
        }
    }

    /// Until when the service asked to be left alone, for a caller that would rather wait than
    /// give up.
    pub fn cool_down_until(&self) -> Option<Instant> {
        *self.cool_down_until.lock()
    }

    fn cooling_down(&self) -> bool {
        self.cool_down_until().is_some_and(|until| Instant::now() < until)
    }

    async fn find_inner(
        &self,
        query: &LyricsQuery,
        ct: &CancellationToken,
    ) -> Result<LyricsLookup, ShapeError> {
        let get = self.send(&self.get_url(query), ct).await;
        if get.transient {
            return Ok(LyricsLookup::failed());
        }
        if let Some(found) = &get.json {
            // LRCLIB's own lookup is close to exact, but not strict about the kind of recording,
            // so its answer is held to the same rule as a search hit.
            if Self::is_this_song(found, query)? {
                return Ok(LyricsLookup::new(Some(Self::parse(found, Some(query))?), false));
            }
        }

        // Then its search, as asked and as the same song written other ways ("suicideboys" for
        // "$uicideboy$"), until one finds it.
        for variant in LyricsIdentity::searches(query, 3) {
            let search = self.send(&self.search_url(&variant), ct).await;
            if search.transient {
                return Ok(LyricsLookup::failed());
            }
            let Some(results) = &search.json else {
                continue;
            };
            if let Some(hit) = Self::pick(results, query)? {
                return Ok(LyricsLookup::new(Some(Self::parse(hit, Some(query))?), false));
            }
        }
        Ok(LyricsLookup::miss())
    }

    fn candidates(results: &[Value]) -> Result<LyricsSearch, ShapeError> {
        let mut candidates = Vec::new();
        for row in results {
            if candidates.len() == 12 {
                break;
            }
            let id = match prop(row, "id")? {
                Some(id @ Value::Number(_)) => get_i64(id)?,
                _ => continue,
            };
            let title = str_prop(row, "trackName")?
                .or(str_prop(row, "name")?)
                .unwrap_or("");
            let mut candidate = LyricsCandidate::new(
                "lrclib",
                id.to_string(),
                title,
                str_prop(row, "artistName")?.unwrap_or(""),
                str_prop(row, "albumName")?.map(str::to_string),
                Self::seconds(row)?.map(round_to_int),
            );
            candidate.lyrics = Some(Self::parse(row, None)?);
            candidates.push(candidate);
        }
        Ok(LyricsSearch::new(candidates, false))
    }

    fn get_url(&self, query: &LyricsQuery) -> String {
        let mut url = format!(
            "{}/api/get?track_name={}&artist_name={}",
            self.base_url,
            escape_data_string(&query.title),
            escape_data_string(&query.artist)
        );
        // An album that is only the title is Octo's own single fallback, not a release LRCLIB knows.
        if let Some(album) = query.album.as_deref()
            && !is_null_or_white_space(Some(album))
            && !eq_ignore_case(album, &query.title)
        {
            url.push_str(&format!("&album_name={}", escape_data_string(album)));
        }
        if let Some(duration) = query.duration_seconds.filter(|d| (1..=3600).contains(d)) {
            url.push_str(&format!("&duration={duration}"));
        }
        url
    }

    fn search_url(&self, query: &SongQuery) -> String {
        format!(
            "{}/api/search?track_name={}&artist_name={}",
            self.base_url,
            escape_data_string(&query.title),
            escape_data_string(&query.artist)
        )
    }

    pub(crate) fn is_this_song(row: &Value, query: &LyricsQuery) -> Result<bool, ShapeError> {
        let title = str_prop(row, "trackName")?.or(str_prop(row, "name")?);
        Ok(LyricsIdentity::same_song(
            &query.title,
            &query.artist,
            title,
            str_prop(row, "artistName")?,
            None::<&[&str]>,
        ) && LyricsIdentity::length_fits(query.duration_seconds, Self::seconds(row)?))
    }

    /// The search hit that is the same song, timed ones first, then the same album, then the
    /// closest length. The search is loose and returns the artist's other songs too.
    pub(crate) fn pick<'a>(results: &'a Value, query: &LyricsQuery) -> Result<Option<&'a Value>, ShapeError> {
        let Value::Array(rows) = results else {
            return Ok(None);
        };
        let mut hits: Vec<(&Value, (u8, u8, f64))> = Vec::new();
        for row in rows {
            if !Self::is_this_song(row, query)? {
                continue;
            }
            let synced = str_prop(row, "syncedLyrics")?.is_some_and(|s| !s.is_empty());
            let plain = str_prop(row, "plainLyrics")?.is_some_and(|s| !s.is_empty());
            let instrumental = matches!(prop(row, "instrumental")?, Some(Value::Bool(true)));
            if !(synced || plain || instrumental) {
                continue;
            }
            // string.Equals(null, null) is true: no album on either side is the same album.
            let same_album = match (str_prop(row, "albumName")?, query.album.as_deref()) {
                (Some(a), Some(b)) => eq_ignore_case(a, b),
                (None, None) => true,
                _ => false,
            };
            let distance = match (query.duration_seconds.filter(|d| *d > 0), Self::seconds(row)?) {
                (Some(want), Some(seconds)) => (seconds - f64::from(want)).abs(),
                _ => 0.0,
            };
            hits.push((row, (u8::from(!synced), u8::from(!same_album), distance)));
        }
        // OrderBy/ThenBy is a stable sort.
        hits.sort_by(|a, b| {
            (a.1.0, a.1.1)
                .cmp(&(b.1.0, b.1.1))
                .then(a.1.2.partial_cmp(&b.1.2).unwrap_or(std::cmp::Ordering::Equal))
        });
        Ok(hits.first().map(|(row, _)| *row))
    }

    /// An entry's lyrics. With hasWordSync, the Lyricsfile's words become enhanced LRC; when
    /// that cannot be read, the entry's own LRC is used as before.
    pub(crate) fn parse(row: &Value, query: Option<&LyricsQuery>) -> Result<LyricsResult, ShapeError> {
        let mut synced = str_prop(row, "syncedLyrics")?.map(str::to_string);
        if matches!(prop(row, "hasWordSync")?, Some(Value::Bool(true)))
            && let Some(lines) = LyricsfileReader::read_lines(str_prop(row, "lyricsfile")?)
            && lines.iter().any(|line| !line.words.is_empty())
        {
            synced = Some(LyricsText::write_lrc(&lines));
        }

        let instrumental = matches!(prop(row, "instrumental")?, Some(Value::Bool(true)));
        let mut result = LyricsResult::new(
            "LRCLIB",
            synced,
            str_prop(row, "plainLyrics")?.map(str::to_string),
            instrumental,
        );
        result.candidate_id = match prop(row, "id")? {
            Some(id @ Value::Number(_)) => Some(format!("lrclib:{}", get_i64(id)?)),
            _ => None,
        };
        result.doubt = match query {
            Some(query) => LyricsIdentity::doubt(query.duration_seconds, Self::seconds(row)?),
            None => None,
        };
        Ok(result)
    }

    fn seconds(row: &Value) -> Result<Option<f64>, ShapeError> {
        Ok(match prop(row, "duration")? {
            Some(Value::Number(number)) => number.as_f64().filter(|seconds| *seconds > 0.0),
            _ => None,
        })
    }

    async fn send(&self, url: &str, ct: &CancellationToken) -> Answer {
        let failed = Answer {
            json: None,
            transient: true,
        };
        let mut last_call = tokio::select! {
            guard = self.gate.lock() => guard,
            () = ct.cancelled() => return failed,
        };
        if let Some(last) = *last_call {
            let gap = (last + Self::MINIMUM_GAP).saturating_duration_since(Instant::now());
            if !gap.is_zero() {
                tokio::select! {
                    () = tokio::time::sleep(gap) => {}
                    () = ct.cancelled() => return failed,
                }
            }
        }
        *last_call = Some(Instant::now());

        let response = tokio::select! {
            response = self.client.get(url).send() => response,
            () = ct.cancelled() => return failed,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                debug!("LRCLIB request failed: {error}");
                return failed;
            }
        };
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Answer {
                json: None,
                transient: false,
            };
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let wait = retry_after_delta(response.headers()).unwrap_or(Self::DEFAULT_COOLDOWN);
            *self.cool_down_until.lock() = Some(Instant::now() + wait);
            info!("LRCLIB asked Octo to wait {}s", wait.as_secs());
            return failed;
        }
        if !status.is_success() {
            return failed;
        }
        let body = tokio::select! {
            body = response.bytes() => body,
            () = ct.cancelled() => return failed,
        };
        match body
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string()))
        {
            Ok(json) => Answer {
                json: Some(json),
                transient: false,
            },
            Err(error) => {
                debug!("LRCLIB request failed: {error}");
                failed
            }
        }
    }
}

#[async_trait]
impl ILyricsSource for LrclibLyricsSource {
    fn key(&self) -> &str {
        "lrclib"
    }

    async fn find(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsLookup {
        if self.cooling_down() {
            return LyricsLookup::failed();
        }
        // An answer of a shape LRCLIB never sends threw in C#, which the service caught as a
        // failed lookup.
        self.find_inner(query, ct)
            .await
            .unwrap_or_else(|_| LyricsLookup::failed())
    }

    async fn search(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsSearch {
        if self.cooling_down() {
            return LyricsSearch::failed();
        }
        // The first search that returns anything: a person choosing wants the entries, and the
        // later searches only exist for a song the first one could not find.
        for variant in LyricsIdentity::searches(query, 3) {
            let search = self.send(&self.search_url(&variant), ct).await;
            if search.transient {
                return LyricsSearch::failed();
            }
            match &search.json {
                Some(Value::Array(rows)) if !rows.is_empty() => {
                    return Self::candidates(rows).unwrap_or_else(|_| LyricsSearch::failed());
                }
                _ => continue,
            }
        }
        LyricsSearch::empty()
    }

    async fn fetch(&self, id: &str, ct: &CancellationToken) -> LyricsLookup {
        let Some(number) = try_parse_long(id) else {
            return LyricsLookup::miss();
        };
        if self.cooling_down() {
            return LyricsLookup::failed();
        }
        let get = self
            .send(&format!("{}/api/get/{number}", self.base_url), ct)
            .await;
        if get.transient {
            return LyricsLookup::failed();
        }
        let Some(found) = &get.json else {
            return LyricsLookup::miss();
        };
        match Self::parse(found, None) {
            Ok(result) => LyricsLookup::new(Some(result), false),
            Err(_) => LyricsLookup::failed(),
        }
    }
}

#[cfg(test)]
#[path = "lrclib_lyrics_source_tests.rs"]
mod tests;
