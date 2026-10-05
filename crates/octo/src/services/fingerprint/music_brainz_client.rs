//! Port of `Services/Fingerprint/MusicBrainzClient.cs`, its HTTP half: the one-a-second gate,
//! the cache and the named "musicbrainz" client (`Program.cs`: base address
//! https://musicbrainz.org/ws/2/, a 10 s timeout, Octo's User-Agent). The query builders and
//! the readers are in `octo_core::fingerprint::music_brainz_client`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::{SongIdentity, dotnet, octo_user_agent};
use octo_core::fingerprint::music_brainz_client as pure;
use octo_core::json::element::ElementError;
use octo_core::tagging::{ReleaseDetails, ReleaseLookup};
use serde_json::Value;
use tokio::time::Instant;
use tracing::debug;
use url::Url;

use crate::services::framework::http::{client_builder, parse_json, resolve};
use crate::services::framework::{HttpAnswer, MemoryCache};

/// What escaped `MusicBrainzClient` in the C#, which callers catch.
#[derive(Debug, thiserror::Error)]
pub enum MusicBrainzError {
    /// The client's timeout elapsed. HttpClient reports that as a `TaskCanceledException`, an
    /// `OperationCanceledException`, which `GetAsync`'s filter does not catch, so it reached
    /// the caller rather than reading as "nothing to say".
    #[error("The request was canceled due to the configured HttpClient.Timeout of 10 seconds elapsing.")]
    TimedOut,
    /// A `JsonElement` accessor threw while the answer was read: a field of the wrong kind.
    #[error(transparent)]
    Malformed(#[from] ElementError),
}

#[derive(Clone)]
enum Cached {
    Release(Arc<ReleaseDetails>),
    Json(Value),
}

/// One question for MusicBrainz, asked only when a person keeps a track AcoustID had never heard
/// of: which recording is this? That is the MusicBrainz id a confirmed fingerprint needs before
/// AcoustID will take it (#47). And one more, asked only while verifying a download whose
/// fingerprint named a recording that reads differently from the request: which ISRCs does
/// that recording carry?
///
/// MusicBrainz allows about one request a second and wants a User-Agent naming the application,
/// and it holds near-duplicate recordings (two "Teardrop" by Massive Attack, 27 ms apart), so an
/// answer is only returned when exactly one recording fits. Anything else is a guess, and a
/// guess is never submitted with someone's name on it.
pub struct MusicBrainzClient {
    client: reqwest::Client,
    base: Url,
    /// `SemaphoreSlim(1, 1)` and `_lastCallUtc`: held across the wait and the call.
    gate: tokio::sync::Mutex<Option<Instant>>,
    /// Release lookups are remembered a day, since an album's tracks ask for the same
    /// release one by one; searches six hours. Every entry counts as one, so the limit is a count.
    cache: MemoryCache<Cached>,
}

impl MusicBrainzClient {
    pub const CLIENT_NAME: &'static str = "musicbrainz";
    pub const BASE_ADDRESS: &'static str = "https://musicbrainz.org/ws/2/";
    const TIMEOUT: Duration = Duration::from_secs(10);
    const MINIMUM_GAP: Duration = Duration::from_millis(1100);
    const RELEASE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    const SEARCH_TTL: Duration = Duration::from_secs(6 * 60 * 60);

    pub fn new() -> Self {
        Self::with_base_url(Url::parse(Self::BASE_ADDRESS).expect("the MusicBrainz base address parses"))
    }

    /// A client for another base address: a test's mock server.
    pub fn with_base_url(base: Url) -> Self {
        let client = client_builder()
            .timeout(Self::TIMEOUT)
            .user_agent(octo_user_agent::value())
            .build()
            .expect("the MusicBrainz HTTP client builds");
        Self {
            client,
            base,
            gate: tokio::sync::Mutex::new(None),
            cache: MemoryCache::new(2000),
        }
    }

    /// One release in full: its label and catalogue number, barcode, status, country, date, its
    /// group's kind and first release date, every track's position and id, and the genres people
    /// voted on. Remembered a day, since an album's tracks ask one by one.
    pub async fn lookup_release(
        &self,
        release_id: &str,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        if dotnet::is_blank(release_id) {
            return Ok(None);
        }
        let key = format!("release|{}", dotnet::to_lower_invariant(release_id.trim()));
        if let Some(Cached::Release(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = format!(
            "release/{}?inc=labels+release-groups+artist-credits+recordings+isrcs+genres&fmt=json",
            dotnet::escape_data_string(release_id.trim())
        );
        let Some(doc) = self.get(&url, "release lookup").await? else {
            return Ok(None);
        };
        let details = ReleaseDetails::parse(&doc).map(Arc::new);
        if let Some(details) = &details {
            self.cache
                .set(key, Cached::Release(Arc::clone(details)), 1, Self::RELEASE_TTL);
        }
        Ok(details)
    }

    /// Recordings by name and length, for a download the fingerprint service could not name. Up
    /// to 25, each with its releases and their groups.
    pub async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> Result<Option<Value>, MusicBrainzError> {
        if dotnet::is_blank(artist) && dotnet::is_blank(title) {
            return Ok(None);
        }
        let key = format!(
            "search|{}|{duration_seconds}",
            SongIdentity::match_key(artist, title)
        );
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = pure::build_recording_search_url(artist, title, duration_seconds);
        let Some(doc) = self.get(&url, "recording search").await? else {
            return Ok(None);
        };
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(Some(doc))
    }

    /// The recordings that carry one code, with their releases.
    pub async fn lookup_isrc(&self, isrc: &str) -> Result<Option<Value>, MusicBrainzError> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return Ok(None);
        };
        let key = format!("isrc|{code}");
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = format!("isrc/{code}?inc=artist-credits+releases+release-groups+media&fmt=json");
        let Some(doc) = self.get(&url, "code lookup").await? else {
            return Ok(None);
        };
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(Some(doc))
    }

    /// The one recording that is this song, this version, by this artist, at this length.
    pub async fn find_recording(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::find_recording_url(artist, title, duration_seconds) else {
            return Ok(None);
        };
        match self.get(&url, "recording search").await? {
            Some(doc) => Ok(pure::pick(&doc, artist, title, duration_seconds)?),
            None => Ok(None),
        }
    }

    /// The ISRCs MusicBrainz lists for one recording: empty when it lists none, None when it
    /// could not be asked. One lookup by id with inc=isrcs, inside the same one-a-second budget
    /// as the search above.
    pub async fn fetch_isrcs(&self, recording_id: &str) -> Result<Option<Vec<String>>, MusicBrainzError> {
        if dotnet::is_blank(recording_id) {
            return Ok(None);
        }
        let url = format!(
            "recording/{}?inc=isrcs&fmt=json",
            dotnet::escape_data_string(recording_id)
        );
        match self.get(&url, "ISRC lookup").await? {
            Some(doc) => Ok(Some(pure::parse_isrcs(&doc)?)),
            None => Ok(None),
        }
    }

    /// The release group of the oldest official studio album a song appears on, or of a
    /// soundtrack when no studio album has it. None when MusicBrainz knows neither.
    pub async fn find_studio_album(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<String>, MusicBrainzError> {
        let Some((url, plain_title)) = pure::studio_album_search(artist, title) else {
            return Ok(None);
        };
        match self.get(&url, "studio album search").await? {
            Some(doc) => Ok(pure::pick_studio_album(&doc, &plain_title)?),
            None => Ok(None),
        }
    }

    /// One request, spaced at least [`Self::MINIMUM_GAP`] after the last. None on any
    /// failure, which every caller reads as "MusicBrainz had nothing to say"; the timeout is
    /// the exception, as it was in the C#.
    async fn get(&self, relative: &str, what: &str) -> Result<Option<Value>, MusicBrainzError> {
        let mut last_call = self.gate.lock().await;
        if let Some(last) = *last_call {
            let due = last + Self::MINIMUM_GAP;
            if due > Instant::now() {
                tokio::time::sleep_until(due).await;
            }
        }
        *last_call = Some(Instant::now());

        let attempt = async {
            let url = resolve(&self.base, relative)?;
            let response = self.client.get(url).send().await?;
            let answer = HttpAnswer::read(response).await?;
            if !answer.is_success() {
                return anyhow::Ok(None);
            }
            Ok(Some(parse_json(&answer.body)?))
        };
        match attempt.await {
            Ok(doc) => Ok(doc),
            Err(e)
                if e.downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_timeout) =>
            {
                Err(MusicBrainzError::TimedOut)
            }
            Err(e) => {
                debug!("musicbrainz {what} failed: {e}");
                Ok(None)
            }
        }
    }
}

/// The music database calls the release identifier makes. A timeout is the error the C#
/// identifier saw thrown; every other failure is already None.
#[async_trait::async_trait]
impl ReleaseLookup for MusicBrainzClient {
    async fn lookup_release(&self, release_id: &str) -> anyhow::Result<Option<ReleaseDetails>> {
        Ok(MusicBrainzClient::lookup_release(self, release_id)
            .await?
            .map(|details| (*details).clone()))
    }

    async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> anyhow::Result<Option<Value>> {
        Ok(MusicBrainzClient::search_recordings(self, artist, title, duration_seconds).await?)
    }

    async fn lookup_isrc(&self, isrc: &str) -> anyhow::Result<Option<Value>> {
        Ok(MusicBrainzClient::lookup_isrc(self, isrc).await?)
    }
}

impl Default for MusicBrainzClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "music_brainz_client_tests.rs"]
mod tests;
