//! Port of `Services/Library/LibraryOwnership.cs`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use octo_core::common::dotnet::{self, escape_data_string, to_lower_invariant};
use octo_core::common::{Clock, SongIdentity};
use octo_core::settings::{LibraryAction, LibraryActionSettings, SettingsStore};
use parking_lot::Mutex;
use serde_json::Value;
use tracing::debug;

use super::NavidromeSongPathResolver;
use super::duplicate_scan_worker::is_lossless_file;
use crate::services::local::ILocalLibraryService;
use crate::services::subsonic::NavidromeIdentityService;

/// A copy of a song already in the library, and whether it is lossless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedCopy {
    pub navidrome_id: Option<String>,
    pub absolute_path: String,
    pub suffix: String,
    pub bit_rate: i32,
    pub lossless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedDecision {
    Download,
    KeepYours,
    KeepAndUpgrade,
}

/// One library song as search3 answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<i32>,
    pub suffix: String,
    pub bit_rate: i32,
}

/// Finds the library's candidates for a query ("{artist} {title}"): `Ok(None)` when Navidrome
/// cannot be asked, `Err` where the C# threw.
pub type SearchFn =
    Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<Option<Vec<Candidate>>>> + Send + Sync>;

/// The verified path of a library song id, or `None`.
pub type ResolveFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;

/// The Navidrome side: the admin identity search3 is signed with, the client, and the resolver
/// that verifies a candidate's file. Absent in tests, which answer through the seams instead.
#[derive(Clone)]
pub struct OwnershipNavidrome {
    pub identity: NavidromeIdentityService,
    pub http: reqwest::Client,
    pub resolver: Arc<NavidromeSongPathResolver>,
}

/// Whether a song is already in the library, so a heart, an album walk or a play never adds a
/// second copy. Octo used to check only its own record of the outside id it downloaded, so a song
/// owned before Octo, or fetched once under another outside id, came down again.
///
/// The same song means the same MatchKey (artist and title, one version: a live take or a remix is
/// its own song), and a length within 8 seconds, or, when a length is unknown, the same album, so
/// one album's "Intro" is never taken for another's.
pub struct LibraryOwnership {
    /// `IOptionsMonitor<SubsonicSettings>`: the URL is read at every search.
    settings: Arc<SettingsStore>,
    navidrome: Option<OwnershipNavidrome>,
    library: Arc<dyn ILocalLibraryService>,
    cache: Mutex<HashMap<String, (DateTime<Utc>, Option<OwnedCopy>)>>,
    seams: Mutex<Seams>,
}

// Seams: tests answer from a list instead of a Navidrome.
struct Seams {
    search: Option<SearchFn>,
    resolve: Option<ResolveFn>,
    clock: Clock,
}

impl LibraryOwnership {
    pub const DURATION_TOLERANCE_SECONDS: i32 = 8;
    const CACHE_FOR: TimeDelta = TimeDelta::minutes(2);

    pub fn new(
        settings: Arc<SettingsStore>,
        navidrome: Option<OwnershipNavidrome>,
        library: Arc<dyn ILocalLibraryService>,
    ) -> Self {
        LibraryOwnership {
            settings,
            navidrome,
            library,
            cache: Mutex::new(HashMap::new()),
            seams: Mutex::new(Seams {
                search: None,
                resolve: None,
                clock: Clock::system(),
            }),
        }
    }

    /// The `Search` seam: answer from a list instead of a Navidrome.
    pub fn set_search(&self, search: SearchFn) {
        self.seams.lock().search = Some(search);
    }

    /// The `Resolve` seam.
    pub fn set_resolve(&self, resolve: ResolveFn) {
        self.seams.lock().resolve = Some(resolve);
    }

    /// The `Clock` seam.
    pub fn set_clock(&self, clock: Clock) {
        self.seams.lock().clock = clock;
    }

    fn now(&self) -> DateTime<Utc> {
        self.seams.lock().clock.now()
    }

    /// The library's copy of this song, the best one when there are several, or `None`.
    pub async fn find(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        duration_seconds: Option<i32>,
        album: Option<&str>,
    ) -> Option<OwnedCopy> {
        let artist = artist.filter(|a| !dotnet::is_blank(a))?;
        let title = title.filter(|t| !dotnet::is_blank(t))?;
        let key = format!(
            "{}|{}|{}",
            SongIdentity::match_key(artist, title),
            duration_seconds.map(|d| d.to_string()).unwrap_or_default(),
            SongIdentity::key(album.unwrap_or_default())
        );
        if let Some((at, copy)) = self.cache.lock().get(&key).cloned()
            && self.now() - at < Self::CACHE_FOR
        {
            return copy;
        }

        let found: anyhow::Result<Option<OwnedCopy>> = async {
            if let Some(copy) = self.in_navidrome(artist, title, duration_seconds, album).await? {
                return Ok(Some(copy));
            }
            Ok(self.in_own_downloads(artist, title, album).await)
        }
        .await;
        let copy = found.unwrap_or_else(|e| {
            // Never a reason to hold a download back: it goes ahead, as before this check existed.
            debug!("Could not tell whether '{artist} - {title}' is owned: {e}");
            None
        });
        self.cache.lock().insert(key, (self.now(), copy.clone()));
        copy
    }

    /// Forget what was found, for a song that has just been placed or replaced.
    pub fn forget(&self) {
        self.cache.lock().clear();
    }

    /// Whether Better quality may run for this person: every gate of the action.
    pub fn upgrade_allowed(actions: &LibraryActionSettings, user: Option<&str>) -> bool {
        !dotnet::is_null_or_white_space(user)
            && actions.enabled
            && !actions.dry_run
            && actions.is_allowed(user)
            && actions
                .effective_actions()
                .iter()
                .any(|a| a.action == LibraryAction::BetterQuality && a.enabled)
    }

    /// What to do with a song about to download. Keep a lossless copy; keep a lossy one and queue
    /// it for Better quality when a lossless source is in the chain and the asker may; keep it
    /// otherwise too, since an MP3 for an MP3 is no upgrade.
    pub fn decide(
        owned: Option<&OwnedCopy>,
        source_can_be_lossless: bool,
        upgrade_allowed: bool,
    ) -> OwnedDecision {
        match owned {
            None => OwnedDecision::Download,
            Some(owned)
                if owned.lossless
                    || !source_can_be_lossless
                    || !upgrade_allowed
                    || owned.navidrome_id.is_none() =>
            {
                OwnedDecision::KeepYours
            }
            Some(_) => OwnedDecision::KeepAndUpgrade,
        }
    }

    pub fn same_song(
        candidate: &Candidate,
        artist: &str,
        title: &str,
        duration_seconds: Option<i32>,
        album: Option<&str>,
    ) -> bool {
        if SongIdentity::match_key(&candidate.artist, &candidate.title)
            != SongIdentity::match_key(artist, title)
        {
            return false;
        }
        if let (Some(wanted), Some(theirs)) = (
            duration_seconds.filter(|d| *d > 0),
            candidate.duration.filter(|d| *d > 0),
        ) {
            return (theirs - wanted).abs() <= Self::DURATION_TOLERANCE_SECONDS;
        }
        let wanted = SongIdentity::key(album.unwrap_or_default());
        !wanted.is_empty() && SongIdentity::key(candidate.album.as_deref().unwrap_or_default()) == wanted
    }

    async fn in_navidrome(
        &self,
        artist: &str,
        title: &str,
        duration: Option<i32>,
        album: Option<&str>,
    ) -> anyhow::Result<Option<OwnedCopy>> {
        let (search, resolve) = {
            let seams = self.seams.lock();
            (seams.search.clone(), seams.resolve.clone())
        };
        let query = format!("{artist} {title}");
        let candidates = match search {
            Some(search) => search(query).await?,
            None => self.search_navidrome(&query).await?,
        };
        let Some(candidates) = candidates else {
            return Ok(None);
        };
        let mut same: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| Self::same_song(c, artist, title, duration, album))
            .collect();
        // OrderByDescending(lossless).ThenByDescending(bit rate), stable.
        same.sort_by(|a, b| {
            is_lossless_file(&b.suffix, b.bit_rate)
                .cmp(&is_lossless_file(&a.suffix, a.bit_rate))
                .then(b.bit_rate.cmp(&a.bit_rate))
        });
        for candidate in same {
            // The verified path, as library actions use: a copy whose file cannot be found is not owned.
            let path = match &resolve {
                Some(resolve) => resolve(candidate.id.clone()).await?,
                None => self.resolve_navidrome(&candidate.id).await,
            };
            let Some(path) = path.filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file())) else {
                continue;
            };
            return Ok(Some(OwnedCopy {
                navidrome_id: Some(candidate.id.clone()),
                absolute_path: path,
                suffix: candidate.suffix.clone(),
                bit_rate: candidate.bit_rate,
                lossless: is_lossless_file(&candidate.suffix, candidate.bit_rate),
            }));
        }
        Ok(None)
    }

    async fn resolve_navidrome(&self, id: &str) -> Option<String> {
        let navidrome = self.navidrome.as_ref()?;
        navidrome.resolver.resolve(id).await.map(|f| f.absolute_path)
    }

    async fn in_own_downloads(&self, artist: &str, title: &str, album: Option<&str>) -> Option<OwnedCopy> {
        let mapping = self
            .library
            .find_mapping_by_tags(Some(artist), Some(title), album)
            .await?;
        let suffix = to_lower_invariant(get_extension(&mapping.local_path).trim_start_matches('.'));
        Some(OwnedCopy {
            navidrome_id: None,
            lossless: is_lossless_file(&suffix, 0),
            absolute_path: mapping.local_path,
            suffix,
            bit_rate: 0,
        })
    }

    async fn search_navidrome(&self, query: &str) -> anyhow::Result<Option<Vec<Candidate>>> {
        let Some(navidrome) = &self.navidrome else {
            return Ok(None);
        };
        let base_url = self.settings.current().subsonic.url.clone().unwrap_or_default();
        if dotnet::is_blank(&base_url) {
            return Ok(None);
        }
        let Some((user, token, salt)) = navidrome.identity.get_scan_auth() else {
            return Ok(None);
        };
        let url = format!(
            "{}/rest/search3?f=json&c=octo&v=1.16.1&query={}&songCount=20&albumCount=0&artistCount=0&u={}&t={token}&s={salt}",
            base_url.trim_end_matches('/'),
            escape_data_string(query),
            escape_data_string(&user),
        );
        let response = navidrome.http.get(&url).send().await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let body = response.bytes().await?;
        let doc: Value = serde_json::from_slice(&body)?;
        Ok(Some(read_search(&doc)?))
    }
}

/// The songs of a search3 answer, read as `JsonElement` did: a node that is not an object where
/// one is asked of throws, and anything missing on the way is no songs.
fn read_search(doc: &Value) -> anyhow::Result<Vec<Candidate>> {
    fn property<'a>(element: &'a Value, name: &str) -> anyhow::Result<Option<&'a Value>> {
        match element {
            Value::Object(map) => Ok(map.get(name)),
            _ => anyhow::bail!("the element is not an object"),
        }
    }
    fn string(element: &Value, name: &str) -> anyhow::Result<Option<String>> {
        Ok(match property(element, name)? {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
    }
    fn int(element: &Value, name: &str) -> anyhow::Result<Option<i32>> {
        Ok(match property(element, name)? {
            Some(Value::Number(n)) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
            _ => None,
        })
    }

    let Some(envelope) = property(doc, "subsonic-response")? else {
        return Ok(Vec::new());
    };
    let Some(result) = property(envelope, "searchResult3")? else {
        return Ok(Vec::new());
    };
    let Some(Value::Array(songs)) = property(result, "song")? else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for song in songs {
        let Some(id) = string(song, "id")?.filter(|id| !id.is_empty()) else {
            continue;
        };
        candidates.push(Candidate {
            id,
            artist: string(song, "artist")?.unwrap_or_default(),
            title: string(song, "title")?.unwrap_or_default(),
            album: string(song, "album")?,
            duration: int(song, "duration")?,
            suffix: string(song, "suffix")?.unwrap_or_default(),
            bit_rate: int(song, "bitRate")?.unwrap_or(0),
        });
    }
    Ok(candidates)
}

/// `Path.GetExtension`: the last dot of the file name and what follows, or empty when the name
/// has no dot or ends with one.
fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

/// A search seam answering from a fixed list, for tests.
#[cfg(test)]
pub(crate) fn search_from(library: Option<Vec<Candidate>>) -> SearchFn {
    use futures::FutureExt;
    Arc::new(move |_| {
        let library = library.clone();
        async move { Ok(library) }.boxed()
    })
}

#[cfg(test)]
#[path = "library_ownership_tests.rs"]
mod tests;
