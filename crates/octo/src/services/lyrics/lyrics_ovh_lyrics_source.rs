//! Port of `Services/Lyrics/LyricsOvhLyricsSource.cs`.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use octo_core::common::dotnet::is_null_or_white_space;
use octo_core::common::{SongIdentity, SongQuery};
use octo_core::lyrics::{
    ILyricsSource, LyricsCandidate, LyricsIdentity, LyricsLookup, LyricsQuery, LyricsResult, LyricsSearch,
};
use reqwest::StatusCode;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::lyrics_http::{escape_data_string, from_base64, str_prop};

/// lyrics.ovh: plain text only, no timing, and it has had outages. It is the last resort,
/// reached only when nothing earlier had lyrics at all. It looks songs up by exact artist and
/// title and names nothing back, so its one entry is the song as asked.
pub struct LyricsOvhLyricsSource {
    /// The `lyrics` client, shared with LRCLIB.
    client: reqwest::Client,
    base_url: String,
}

impl LyricsOvhLyricsSource {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.lyrics.ovh";

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

    /// The song as asked, then written the other ways [`LyricsIdentity::searches`] gives
    /// ("suicideboys" for "$uicideboy$", the primary artist alone). lyrics.ovh names nothing
    /// back, so nothing it answers can be checked: only a variant that is the same song by
    /// construction is tried, and none for a title that names a version ("Song (Live)"), whose
    /// cleaned query would ask for the original.
    async fn look_up(&self, query: &LyricsQuery, ct: &CancellationToken) -> (LyricsLookup, SongQuery) {
        let mut searches = LyricsIdentity::searches(query, 3);
        if !SongIdentity::distinct_versions(
            &SongIdentity::parse_title(&query.title, Some(&query.artist)),
            None,
        )
        .is_empty()
        {
            searches.truncate(1);
        }
        let mut last = (LyricsLookup::miss(), searches[0].clone());
        for search in searches {
            last = (self.get(&search.artist, &search.title, ct).await, search);
            if last.0.transient || last.0.result.is_some() {
                break;
            }
        }
        last
    }

    /// The artist and the title, which are all lyrics.ovh looks a song up by.
    fn id_of(artist: &str, title: &str) -> String {
        URL_SAFE_NO_PAD.encode(format!("{artist}\n{title}"))
    }

    async fn get(&self, artist: &str, title: &str, ct: &CancellationToken) -> LyricsLookup {
        let url = format!(
            "{}/v1/{}/{}",
            self.base_url,
            escape_data_string(artist),
            escape_data_string(title)
        );
        let response = tokio::select! {
            response = self.client.get(&url).send() => response,
            () = ct.cancelled() => return LyricsLookup::failed(),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                debug!("lyrics.ovh request failed: {error}");
                return LyricsLookup::failed();
            }
        };
        if response.status() == StatusCode::NOT_FOUND {
            return LyricsLookup::miss();
        }
        if !response.status().is_success() {
            return LyricsLookup::failed();
        }
        let body = tokio::select! {
            body = response.bytes() => body,
            () = ct.cancelled() => return LyricsLookup::failed(),
        };
        let read = body
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string()))
            .and_then(|doc| {
                str_prop(&doc, "lyrics")
                    .map(|text| text.map(|text| text.replace("\r\n", "\n").trim().to_string()))
                    .map_err(|error| error.to_string())
            });
        match read {
            Ok(text) if !is_null_or_white_space(text.as_deref()) => {
                let mut result = LyricsResult::new("lyrics.ovh", None, text, false)
                    .with_candidate_id(format!("lyricsovh:{}", Self::id_of(artist, title)));
                result.doubt = Some("lyrics.ovh names no song and no length".to_string());
                LyricsLookup::new(Some(result), false)
            }
            Ok(_) => LyricsLookup::miss(),
            Err(error) => {
                debug!("lyrics.ovh request failed: {error}");
                LyricsLookup::failed()
            }
        }
    }
}

#[async_trait]
impl ILyricsSource for LyricsOvhLyricsSource {
    fn key(&self) -> &str {
        "lyricsovh"
    }

    async fn find(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsLookup {
        self.look_up(query, ct).await.0
    }

    async fn search(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsSearch {
        let (lookup, asked) = self.look_up(query, ct).await;
        if lookup.transient {
            return LyricsSearch::failed();
        }
        let Some(result) = lookup.result else {
            return LyricsSearch::empty();
        };
        let mut candidate = LyricsCandidate::new(
            "lyricsovh",
            Self::id_of(&asked.artist, &asked.title),
            asked.title.clone(),
            asked.artist.clone(),
            None,
            None,
        );
        candidate.lyrics = Some(result);
        LyricsSearch::new(vec![candidate], false)
    }

    async fn fetch(&self, id: &str, ct: &CancellationToken) -> LyricsLookup {
        // The id's URL-safe base64, padded back as C# padded it (by the id's own length).
        let padded_length = id.chars().count().div_ceil(4) * 4;
        let mut standard: String = id
            .chars()
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                other => other,
            })
            .collect();
        while standard.chars().count() < padded_length {
            standard.push('=');
        }
        let Some(bytes) = from_base64(&standard) else {
            return LyricsLookup::miss();
        };
        let text = String::from_utf8_lossy(&bytes);
        match text.split_once('\n') {
            Some((artist, title)) => self.get(artist, title, ct).await,
            None => LyricsLookup::miss(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::lyrics::kugou_lyrics_source::tests::routes;
    use crate::services::lyrics::lyrics_http::lyrics_http_client;

    fn source(http: &crate::services::lyrics::test_support::FakeHttp) -> LyricsOvhLyricsSource {
        LyricsOvhLyricsSource::with_base_url(lyrics_http_client(), &http.uri())
    }

    // QueryVariantLookupTests.LyricsOvh_TriesTheSpelledOutName
    #[tokio::test]
    async fn lyrics_ovh_tries_the_spelled_out_name() {
        let http = routes(&[("/v1/suicideboys/SUICIDE", r#"{"lyrics":"Line one"}"#.into())]).await;

        let lookup = source(&http)
            .find(
                &LyricsQuery::new("$uicideboy$", "$UICIDE", None, Some(170)),
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(lookup.result.and_then(|r| r.plain).as_deref(), Some("Line one"));
    }

    // QueryVariantLookupTests.LyricsOvh_NeverAsksForTheOriginalOfAVersion
    #[tokio::test]
    async fn lyrics_ovh_never_asks_for_the_original_of_a_version() {
        // lyrics.ovh names nothing back, so its answer to "Creep" could not be told from the
        // live take's: a title naming a version is asked for as it is, once.
        let http = routes(&[("/v1/Radiohead/Creep$", r#"{"lyrics":"studio words"}"#.into())]).await;

        let lookup = source(&http)
            .find(
                &LyricsQuery::new("Radiohead", "Creep (Live)", None, Some(240)),
                &CancellationToken::new(),
            )
            .await;

        assert!(lookup.result.is_none());
        assert_eq!(http.calls().await.len(), 1);
    }

    #[tokio::test]
    async fn the_one_entry_is_the_song_as_asked_and_its_id_fetches_it_again() {
        let http = routes(&[(
            "/v1/Björk/Jóga",
            "{\"lyrics\":\"All these accidents\\r\\n \"}".into(),
        )])
        .await;
        let ovh = source(&http);

        let search = ovh
            .search(
                &LyricsQuery::new("Björk", "Jóga", None, None),
                &CancellationToken::new(),
            )
            .await;
        let entry = &search.candidates[0];
        assert_eq!(entry.id, LyricsOvhLyricsSource::id_of("Björk", "Jóga"));
        assert!(!entry.id.contains('='));
        let lyrics = entry.lyrics.as_ref().expect("carried");
        assert_eq!(lyrics.plain.as_deref(), Some("All these accidents"));
        assert_eq!(lyrics.candidate_id, Some(entry.candidate_id()));

        let again = ovh.fetch(&entry.id, &CancellationToken::new()).await;
        assert_eq!(
            again.result.and_then(|r| r.plain).as_deref(),
            Some("All these accidents")
        );
        assert_eq!(
            ovh.fetch("%%%", &CancellationToken::new()).await,
            LyricsLookup::miss()
        );
        assert_eq!(
            ovh.fetch(&URL_SAFE_NO_PAD.encode("no newline"), &CancellationToken::new())
                .await,
            LyricsLookup::miss()
        );
    }
}
