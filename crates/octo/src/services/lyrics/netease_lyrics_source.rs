//! Port of `Services/Lyrics/NeteaseLyricsSource.cs`.

use async_trait::async_trait;
use octo_core::common::dotnet::is_null_or_white_space;
use octo_core::lyrics::{
    ILyricsSource, LyricsCandidate, LyricsIdentity, LyricsLookup, LyricsQuery, LyricsResult, LyricsSearch,
    LyricsText,
};
use reqwest::header::REFERER;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::lyrics_http::{
    ShapeError, escape_data_string, get_i64, get_string, prop, round_to_int, try_parse_long,
};

/// NetEase Cloud Music: much deeper than LRCLIB on non-Western and older music, and often
/// synced. Its API is undocumented and unlicensed, so it only runs when LYRICS_SOURCES names
/// it. Every lyric opens with contributor credits, which `LyricsText.StripCredits` removes.
pub struct NeteaseLyricsSource {
    /// The `lyrics` client, shared with LRCLIB.
    client: reqwest::Client,
    base_url: String,
}

/// Why a request gave no JSON: the C# answered a non-success status with null, and let
/// anything else throw to the method's catch-all.
enum Failure {
    /// Not a success status.
    Status,
    /// The request failed, or the answer was not JSON or not of the expected shape.
    Error(String),
    Cancelled,
}

impl From<ShapeError> for Failure {
    fn from(error: ShapeError) -> Self {
        Failure::Error(error.to_string())
    }
}

impl NeteaseLyricsSource {
    pub const DEFAULT_BASE_URL: &'static str = "https://music.163.com";

    /// The source over the `lyrics` client ([`super::lyrics_http::lyrics_http_client`]).
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_base_url(client, Self::DEFAULT_BASE_URL)
    }

    /// The same against another address, for tests.
    pub fn with_base_url(client: reqwest::Client, base_url: &str) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// The lyrics of the first entry in a search already made that is this song.
    pub async fn find_in(
        &self,
        query: &LyricsQuery,
        search: &LyricsSearch,
        ct: &CancellationToken,
    ) -> LyricsLookup {
        if search.transient {
            return LyricsLookup::failed();
        }
        let Some(hit) = search
            .candidates
            .iter()
            .find(|candidate| Self::is_this_song(candidate, query))
        else {
            return LyricsLookup::miss();
        };

        let lookup = self.fetch(&hit.id, ct).await;
        match lookup.result {
            Some(mut result) => {
                result.doubt =
                    LyricsIdentity::doubt(query.duration_seconds, hit.duration_seconds.map(f64::from));
                LyricsLookup::new(Some(result), false)
            }
            None => lookup,
        }
    }

    pub fn is_this_song(candidate: &LyricsCandidate, query: &LyricsQuery) -> bool {
        let credits: Vec<&str> = candidate
            .artist
            .split(" & ")
            .filter(|credit| !credit.is_empty())
            .collect();
        LyricsIdentity::same_song(
            &query.title,
            &query.artist,
            Some(&candidate.title),
            Some(&candidate.artist),
            Some(credits.as_slice()),
        ) && LyricsIdentity::length_fits(query.duration_seconds, candidate.duration_seconds.map(f64::from))
    }

    async fn search_inner(
        &self,
        query: &LyricsQuery,
        ct: &CancellationToken,
    ) -> Result<LyricsSearch, Failure> {
        let mut candidates: Vec<LyricsCandidate> = Vec::new();
        for variant in LyricsIdentity::searches(query, 3) {
            let url = format!(
                "{}/api/search/get?s={}&type=1&limit=8",
                self.base_url,
                escape_data_string(&variant.text())
            );
            let search = match self.get_json(&url, ct).await {
                Ok(search) => search,
                Err(Failure::Status) => {
                    return Ok(if candidates.is_empty() {
                        LyricsSearch::failed()
                    } else {
                        LyricsSearch::new(candidates, true)
                    });
                }
                Err(other) => return Err(other),
            };
            // Each one only once, even within one answer.
            for found in Self::read(&search)? {
                if candidates.iter().all(|seen| seen.id != found.id) {
                    candidates.push(found);
                }
            }
            if candidates
                .iter()
                .any(|candidate| Self::is_this_song(candidate, query))
            {
                break;
            }
        }
        Ok(LyricsSearch::new(candidates, false))
    }

    async fn fetch_inner(&self, number: i64, ct: &CancellationToken) -> Result<LyricsLookup, Failure> {
        let url = format!("{}/api/song/lyric?id={number}&lv=1&kv=1&tv=-1", self.base_url);
        let lyric = match self.get_json(&url, ct).await {
            Ok(lyric) => lyric,
            Err(Failure::Status) => return Ok(LyricsLookup::failed()),
            Err(other) => return Err(other),
        };

        let raw = match prop(&lyric, "lrc")? {
            Some(lrc) => match prop(lrc, "lyric")? {
                Some(Value::String(body)) => Some(body.as_str()),
                _ => None,
            },
            None => None,
        };
        let Some(raw) = raw.filter(|raw| !is_null_or_white_space(Some(raw))) else {
            return Ok(LyricsLookup::miss());
        };

        let clean = LyricsText::strip_credits(raw);
        if is_null_or_white_space(Some(&clean)) {
            return Ok(LyricsLookup::miss());
        }
        let result = if LyricsText::has_timestamps(Some(&clean)) {
            LyricsResult::new("NetEase", Some(clean), None, false)
        } else {
            LyricsResult::new("NetEase", None, Some(clean), false)
        };
        Ok(LyricsLookup::new(
            Some(result.with_candidate_id(format!("netease:{number}"))),
            false,
        ))
    }

    /// The search's songs, in NetEase's order.
    pub(crate) fn read(root: &Value) -> Result<Vec<LyricsCandidate>, ShapeError> {
        let Some(result) = prop(root, "result")? else {
            return Ok(Vec::new());
        };
        let Some(Value::Array(songs)) = prop(result, "songs")? else {
            return Ok(Vec::new());
        };

        let mut candidates = Vec::new();
        for song in songs {
            let id = match prop(song, "id")? {
                Some(id @ Value::Number(_)) => get_i64(id)?,
                _ => continue,
            };
            let mut artists = Vec::new();
            if let Some(Value::Array(list)) = prop(song, "artists")? {
                for artist in list {
                    let name = match prop(artist, "name")? {
                        Some(name) => get_string(name)?.unwrap_or(""),
                        None => "",
                    };
                    if !name.is_empty() {
                        artists.push(name);
                    }
                }
            }
            let album = match prop(song, "album")? {
                Some(album @ Value::Object(_)) => match prop(album, "name")? {
                    Some(name) => get_string(name)?.map(str::to_string),
                    None => None,
                },
                _ => None,
            };
            let seconds = match prop(song, "duration")? {
                Some(Value::Number(duration)) => duration
                    .as_f64()
                    .filter(|ms| *ms > 0.0)
                    .map(|ms| round_to_int(ms / 1000.0)),
                _ => None,
            };
            let name = match prop(song, "name")? {
                Some(name) => get_string(name)?.unwrap_or(""),
                None => "",
            };
            candidates.push(LyricsCandidate::new(
                "netease",
                id.to_string(),
                name,
                artists.join(" & "),
                album,
                seconds,
            ));
        }
        Ok(candidates)
    }

    /// The id of the first result that is the same song, by the same artist, of the same length.
    /// (Internal in C# and called by nothing there either; kept for its tests.)
    #[allow(dead_code)]
    pub(crate) fn pick(root: &Value, query: &LyricsQuery) -> Result<Option<i64>, ShapeError> {
        Ok(Self::read(root)?
            .into_iter()
            .find(|candidate| Self::is_this_song(candidate, query))
            .and_then(|hit| hit.id.parse().ok()))
    }

    async fn get_json(&self, url: &str, ct: &CancellationToken) -> Result<Value, Failure> {
        let request = self.client.get(url).header(REFERER, "https://music.163.com/");
        let response = tokio::select! {
            response = request.send() => response,
            () = ct.cancelled() => return Err(Failure::Cancelled),
        };
        let response = response.map_err(|error| Failure::Error(error.to_string()))?;
        if !response.status().is_success() {
            return Err(Failure::Status);
        }
        let body = tokio::select! {
            body = response.bytes() => body,
            () = ct.cancelled() => return Err(Failure::Cancelled),
        };
        let bytes = body.map_err(|error| Failure::Error(error.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|error| Failure::Error(error.to_string()))
    }
}

#[async_trait]
impl ILyricsSource for NeteaseLyricsSource {
    fn key(&self) -> &str {
        "netease"
    }

    async fn find(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsLookup {
        let search = self.search(query, ct).await;
        self.find_in(query, &search, ct).await
    }

    /// The songs a search for "artist title" returns, and for the same song written the ways
    /// [`LyricsIdentity::searches`] gives, until one of them is this song.
    async fn search(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsSearch {
        match self.search_inner(query, ct).await {
            Ok(search) => search,
            Err(Failure::Error(message)) => {
                debug!("NetEase search failed: {message}");
                LyricsSearch::failed()
            }
            Err(_) => LyricsSearch::failed(),
        }
    }

    async fn fetch(&self, id: &str, ct: &CancellationToken) -> LyricsLookup {
        let Some(number) = try_parse_long(id) else {
            return LyricsLookup::miss();
        };
        match self.fetch_inner(number, ct).await {
            Ok(lookup) => lookup,
            Err(Failure::Error(message)) => {
                debug!("NetEase lyrics request failed: {message}");
                LyricsLookup::failed()
            }
            Err(_) => LyricsLookup::failed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::lyrics::kugou_lyrics_source::tests::{OTHER_VERSIONS, routes};
    use crate::services::lyrics::lyrics_http::lyrics_http_client;

    // SongIdentityTests.Lyrics_NetEase_NeverTakesAnotherVersion (7 rows)
    #[test]
    fn lyrics_netease_never_takes_another_version() {
        for (want, got) in OTHER_VERSIONS {
            assert!(
                !NeteaseLyricsSource::is_this_song(
                    &LyricsCandidate::new("netease", "1", got, "Artist", None, Some(200)),
                    &LyricsQuery::new("Artist", want, None, Some(200))
                ),
                "{want} / {got}"
            );
        }
    }

    // QueryVariantLookupTests.NetEase_SearchesAgainUntilTheSongIsThere
    #[tokio::test]
    async fn netease_searches_again_until_the_song_is_there() {
        let http = routes(&[
            (
                "s=suicideboys SUICIDE",
                r#"{"result":{"songs":[{"id":7,"name":"Suicide","artists":[{"name":"Suicideboys"}],"duration":170000}]}}"#.into(),
            ),
            (
                "/api/search/get",
                r#"{"result":{"songs":[{"id":8,"name":"Ultimate $uicide","artists":[{"name":"$uicideboy$"}],"duration":170000}]}}"#.into(),
            ),
            ("/api/song/lyric", r#"{"lrc":{"lyric":"[00:01.00]Line one"}}"#.into()),
        ])
        .await;
        let source = NeteaseLyricsSource::with_base_url(lyrics_http_client(), &http.uri());

        let lookup = source
            .find(
                &LyricsQuery::new("$uicideboy$", "$UICIDE", None, Some(170)),
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(
            lookup.result.and_then(|r| r.candidate_id).as_deref(),
            Some("netease:7")
        );
        let calls = http.calls().await;
        assert_eq!(
            calls.iter().filter(|c| c.contains("/api/search/get")).count(),
            2,
            "{calls:?}"
        );
    }

    #[test]
    fn read_takes_numbered_songs_their_artists_album_and_length() {
        let root: Value = serde_json::from_str(
            r#"{"result":{"songs":[
                {"id":1,"name":"A","artists":[{"name":"X"},{"name":""},{"name":"Y"}],"album":{"name":"LP"},"duration":200500},
                {"id":"2","name":"skipped"},
                {"id":3,"name":null,"album":"not an object","duration":0},
                {"id":1,"name":"again"}]}}"#,
        )
        .expect("json");
        let read = NeteaseLyricsSource::read(&root).expect("read");
        assert_eq!(read.len(), 3);
        assert_eq!(read[0].artist, "X & Y");
        assert_eq!(read[0].album.as_deref(), Some("LP"));
        // Math.Round(200.5) is 200, to even.
        assert_eq!(read[0].duration_seconds, Some(200));
        assert_eq!(read[1].title, "");
        assert_eq!(read[1].duration_seconds, None);
        assert_eq!(
            NeteaseLyricsSource::pick(&root, &LyricsQuery::new("X", "A", None, Some(200))),
            Ok(Some(1))
        );
        // A name that is not a string threw in C#.
        let odd: Value = serde_json::from_str(r#"{"result":{"songs":[{"id":1,"name":5}]}}"#).expect("json");
        assert!(NeteaseLyricsSource::read(&odd).is_err());
    }

    #[tokio::test]
    async fn fetch_strips_the_credits_and_tells_plain_from_timed() {
        let http = routes(&[
            (
                "id=1&",
                r#"{"lrc":{"lyric":"[00:00.00] 作词 : 周杰伦\n[00:20.50]窗外的麻雀"}}"#.into(),
            ),
            ("id=2&", r#"{"lrc":{"lyric":"just words"}}"#.into()),
            ("id=3&", r#"{"nolyric":true}"#.into()),
            ("id=4&", r#"{"lrc":null}"#.into()),
        ])
        .await;
        let source = NeteaseLyricsSource::with_base_url(lyrics_http_client(), &http.uri());
        let none = CancellationToken::new();

        let timed = source.fetch("1", &none).await.result.expect("found");
        assert_eq!(timed.synced.as_deref(), Some("[00:20.50]窗外的麻雀"));
        assert_eq!(timed.candidate_id.as_deref(), Some("netease:1"));
        let plain = source.fetch("2", &none).await.result.expect("found");
        assert_eq!(plain.plain.as_deref(), Some("just words"));
        assert_eq!(source.fetch("3", &none).await, LyricsLookup::miss());
        // "lrc": null is not an object: C# threw, and caught it as a failure.
        assert_eq!(source.fetch("4", &none).await, LyricsLookup::failed());
        // Not found is a failure here, not a miss.
        assert_eq!(source.fetch("5", &none).await, LyricsLookup::failed());
        assert_eq!(source.fetch("x", &none).await, LyricsLookup::miss());
    }
}
