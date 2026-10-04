//! Port of `Services/Soulseek/SoulseekMetadataService.cs`: the service. The routing model at the
//! bottom of that file is `octo_core::soulseek::soulseek_metadata_service`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use futures::FutureExt;
use futures::StreamExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::{SongIdentity, dotnet};
use octo_core::metadata::deezer_metadata_service::{
    AlbumAnswer, AlbumHit, ArtistHit, TrackMeta, release_types,
};
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use octo_core::soulseek::{LengthSource, RoutingKind, SongLength, SoulseekRouting};
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use super::external_id_registry::{ExternalIdRegistry, SharedRouting};
use crate::services::cover_art::CoverArtAggregator;
use crate::services::framework::InFlight;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::metadata::DeezerMetadataService;
use crate::services::you_tube::YouTubeResolver;

/// The two members of `LastFmService` the length lookup uses: whether it has an API key, and
/// the duration `track.getInfo` gives a song, in seconds. A seam rather than the service, which
/// task 3-B ports.
///
/// Implemented by `LastFmService`; tests substitute a fake.
#[async_trait]
pub trait LastFmTrackLengths: Send + Sync {
    /// `LastFmService.HasApiKey`.
    fn has_api_key(&self) -> bool;

    /// `(await GetTrackInfoAsync(artist, title, ct))?.Duration`.
    async fn track_duration(&self, artist: &str, title: &str) -> Option<i32>;
}

/// Music metadata service for the YouTube-first / Soulseek-on-star architecture.
///
/// Radio queue creation is YouTube-only and lightweight: one yt-dlp search per
/// Last.fm similar track. We do NOT query Soulseek here — Soulseek is reserved
/// for the explicit "user wants to keep this" action (star / permanent download)
/// in SoulseekDownloadService.
///
/// External IDs are kept short (~30-80 chars) so Subsonic clients accept them.
/// Format:  yt|{videoId}|{artist_b64}|{title_b64}|{durationSec}
///
/// Cheap to clone: the clones share their gates and walks.
#[derive(Clone)]
pub struct SoulseekMetadataService {
    inner: Arc<Inner>,
}

/// An artist's releases and the track counts shown beside them.
#[derive(Debug, Clone, Default)]
struct ArtistWalk {
    releases: Vec<AlbumHit>,
    counts: Vec<Option<i32>>,
}

struct Inner {
    youtube: Arc<YouTubeResolver>,
    id_registry: Arc<ExternalIdRegistry>,
    deezer: Arc<DeezerMetadataService>,
    cover_art: Arc<CoverArtAggregator>,
    last_fm: Option<Arc<dyn LastFmTrackLengths>>,

    /// Ids with a lookup queued or running, so several clients loading one station
    /// at once do not each look its songs up again.
    length_lookups: Mutex<HashSet<String>>,

    /// The most recent background lookup, so a test can wait for it.
    last_length_warm: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,

    // Shared across ALL invocations, not created per call. The shim runs 5
    // yt-dlp processes at a time; per-invocation semaphores let the three
    // prewarm triggers (radio, scrobble, external search) stack to 12
    // concurrent /search against it, and the old value of 6 here exceeded the
    // whole gate on its own. Sized to the shim's background capacity
    // (MAX_CONCURRENT_YTDLP - GATE_RESERVE_INTERACTIVE). This service is
    // registered as a singleton, so an instance field is already process-wide
    // without being static (which would make parallel test runs hostile).
    prewarm_gate: Semaphore,

    // Cover art never touches the shim: it hits Deezer/iTunes/Last.fm over HTTP, and
    // Deezer's own background lane (DeezerRateLimiter.BackgroundPermits) already bounds
    // that traffic. It needs its own gate, not prewarm_gate above: sharing that one meant
    // 24 cover-art tasks and 12 YouTube tasks fought over 3 permits with a 2s bounded
    // wait, so most cover fetches timed out and the ones that won starved YouTube prewarm.
    cover_art_prewarm_gate: Semaphore,

    // The length pass that runs after a search has its own, smaller gate. On prewarm_gate its
    // eight lookups took every permit, and a getSong right after the search waited out its two
    // seconds and showed Deezer's length instead of the video's.
    background_duration_gate: Semaphore,

    /// The catalog artist each library artist's page settled on, by the page's artist
    /// and library titles. Never on the shared routing: see find_artist_releases.
    library_picks: Mutex<HashMap<String, String>>,

    /// Walks of an artist's catalog in progress: the releases alone, and the releases with
    /// their counts filled. Keyed by the artist's id and the library titles, since those decide
    /// which artist of a name is meant. A client opens an artist's page with two requests at
    /// once, and each used to walk the catalog on its own: twice the calls against a quota
    /// search and playback share. Everything a walk asks is cached once it is back, so a
    /// finished walk leaves this and the next visit reads the cache.
    release_walks: InFlight<Vec<AlbumHit>>,
    count_walks: InFlight<ArtistWalk>,
}

/// Deezer's real ceiling is ~50 requests per 5 seconds, and this runs on search3's
/// critical path, so the set that blocks a response stays small. 60 rows at 8-way
/// concurrency was roughly 65 requests/second on its own, which is what exhausted the
/// quota and poisoned the metadata caches (issue #8). DeezerRateLimiter now holds that
/// budget centrally, so this figure is about how long a user waits, not about safety.
///
/// 12 is the same "first page" figure PrewarmYouTubeIdsAsync already uses. It must
/// stay above TopDurationResolveLimit, or the rows that get a YouTube length hint
/// would be reading a duration nobody resolved.
const SEARCH_ENRICH_LIMIT: usize = 12;

/// The whole slice a search can return. Everything between the first page and this is
/// filled from cache and warmed for next time, because a row with no enrichment falls
/// back to a flat 180 and a page of identical 3:00 rows is worse than a page of
/// approximate ones. Deezer's length is the approximation - it is not always the
/// recording that plays - and the exact value is resolved when a row is opened or
/// played, where getSong and the native detail endpoint both call
/// ResolveTopDurationsAsync.
const BACKGROUND_ENRICH_LIMIT: usize = 60;

// ---- Lengths for rows that went out without one ---------------------------------
//
// About half of all outside songs reached clients with the 180s placeholder: search
// rows past the first page, and every station row Last.fm gave no length for. The
// station path never looked a length up at all, and what the search warm fetched only
// reached Deezer's in-memory cache, so a song got its length back on a repeat search at
// best and lost it again on a restart.
//
// Nothing here holds up a response. The rows a response can complete for free
// (registry, Deezer's cache) are completed inline; the rest are looked up in the
// background and stored on the registry, so the NEXT response for the song carries the
// length. Sources are tried in order of how far their length can be trusted, and the
// first one to answer ends the chain.

/// Most rows one station response queues for a lookup. The next response
/// queues the next ones, since those done by then are no longer cold.
const STATION_LENGTH_WARM_LIMIT: usize = 20;

/// Ceiling on one song's lookup chain. Last.fm has no client timeout of its
/// own, and one hung call would otherwise stall every song queued behind it.
const LENGTH_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

// Resolve the ACTUAL YouTube video for the top of the list at search time and
// use its duration. Deezer's duration is a different recording (e.g. "Fade"
// is 3:13 on Deezer but the YouTube upload that plays is 3:45), so the scrub
// bar overran and the client's advance logic broke. Storing the videoId also
// means playback reuses this exact video (durations match) and it is prewarmed.
const TOP_DURATION_RESOLVE_LIMIT: usize = 8;

const PREWARM_QUEUE_WAIT: Duration = Duration::from_secs(2);

/// How long an artist's page waits for its albums' track counts.
const TRACK_COUNT_WAIT: Duration = Duration::from_secs(2);

/// How many unknown track counts one visit to an artist's page asks for, and how
/// many at once: gentle on the catalog's quota, which search and playback share.
const TRACK_COUNTS_PER_VISIT: usize = 20;
const TRACK_COUNTS_AT_ONCE: usize = 4;

/// How many catalog artists a name search weighs. The first hit is not reliably
/// the artist asked for: a name another artist shares, or a bigger act that contains it,
/// can come first.
const ARTIST_CANDIDATES: i32 = 5;

/// How many artists of one name a library artist's page compares against the
/// library. Each is one listing call, cached, and only made when a name is shared.
const ARTISTS_COMPARED: usize = 3;

/// How many library pages' choices are kept. Past it they start over: a choice
/// lost costs one comparison against listings the catalog cache still holds.
const LIBRARY_PICKS_KEPT: usize = 2048;

/// The songs-filed-under lookup's limit (`SongsFiledUnder`'s default).
const SONGS_FILED_UNDER_LIMIT: usize = 50;

/// A permit from `gate`, or none when it is not free within the bounded queue wait.
async fn bounded_wait(gate: &Semaphore) -> Option<tokio::sync::SemaphorePermit<'_>> {
    tokio::time::timeout(PREWARM_QUEUE_WAIT, gate.acquire())
        .await
        .ok()
        .and_then(Result::ok)
}

/// `string.Equals(a, "soulseek", OrdinalIgnoreCase)`.
fn is_ours(external_provider: &str) -> bool {
    dotnet::eq_ignore_case(external_provider, SoulseekMetadataService::PROVIDER_NAME)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// A catalog record type as release types, a fresh list for each album.
fn release_type_list(record_type: Option<&str>) -> Vec<String> {
    release_types(record_type).iter().map(|t| t.to_string()).collect()
}

/// How many of an artist's releases the library holds, by the matcher's key.
fn shared_count(releases: &[AlbumHit], owned: &HashSet<String>) -> usize {
    releases
        .iter()
        .filter(|release| owned.contains(&SongIdentity::key(&release.title)))
        .count()
}

/// The artist more people follow; the catalog's own order on a tie.
fn most_followed<'a>(hits: impl IntoIterator<Item = &'a ArtistHit>) -> Option<&'a ArtistHit> {
    let mut best: Option<&ArtistHit> = None;
    for hit in hits {
        if best.is_none_or(|b| hit.fans > b.fans) {
            best = Some(hit);
        }
    }
    best
}

/// The catalog artists a name search answered that bear exactly this name.
fn same_name(hits: Vec<ArtistHit>, name: &str) -> Vec<ArtistHit> {
    hits.into_iter()
        .filter(|hit| SongIdentity::same_artist_name(&hit.name, name))
        .collect()
}

/// `Convert.FromBase64String`, which ignored whitespace and the unused bits of the last
/// character.
const DOTNET_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

fn b64_url_encode(s: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(s.as_bytes())
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

fn b64_url_decode(s: &str) -> Option<String> {
    let mut s = s.replace('-', "+").replace('_', "/");
    match s.len() % 4 {
        2 => s.push_str("=="),
        3 => s.push('='),
        _ => {}
    }
    let compact: String = s
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect();
    let bytes = DOTNET_BASE64.decode(compact).ok()?;
    // Encoding.UTF8.GetString replaces what is not UTF-8.
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `int.TryParse` with the default style: surrounding white space and a sign allowed.
fn parse_int(text: &str) -> Option<i32> {
    text.trim_matches(|c: char| matches!(c, '\u{9}'..='\u{D}' | ' '))
        .parse()
        .ok()
}

/// "artist title..." split at the first space; the title is None for a single word.
fn parse_query(query: &str) -> (String, Option<String>) {
    let trimmed = query.trim();
    match trimmed.find(' ') {
        Some(idx) if idx > 0 => (
            trimmed[..idx].to_string(),
            Some(trimmed[idx + 1..].trim().to_string()),
        ),
        _ => (trimmed.to_string(), None),
    }
}

impl SoulseekMetadataService {
    pub const PROVIDER_NAME: &'static str = "soulseek";

    pub fn new(
        youtube: Arc<YouTubeResolver>,
        id_registry: Arc<ExternalIdRegistry>,
        deezer: Arc<DeezerMetadataService>,
        cover_art: Arc<CoverArtAggregator>,
        last_fm: Option<Arc<dyn LastFmTrackLengths>>,
    ) -> Self {
        SoulseekMetadataService {
            inner: Arc::new(Inner {
                youtube,
                id_registry,
                deezer,
                cover_art,
                last_fm,
                length_lookups: Mutex::new(HashSet::new()),
                last_length_warm: Mutex::new(None),
                prewarm_gate: Semaphore::new(3),
                cover_art_prewarm_gate: Semaphore::new(6),
                background_duration_gate: Semaphore::new(2),
                library_picks: Mutex::new(HashMap::new()),
                release_walks: InFlight::new(),
                count_walks: InFlight::new(),
            }),
        }
    }

    /// The most recent background length lookup, for a test to wait on (`LastLengthWarm`).
    pub async fn last_length_warm(&self) {
        let warm = self.inner.last_length_warm.lock().clone();
        if let Some(warm) = warm {
            warm.await;
        }
    }

    // ====== Short opaque ID format ======
    // Pipe-delimited fields, base64url where needed.
    //   yt|{videoId}|{artistB64}|{titleB64}|{durationSec}
    // Total length ~30-80 chars depending on artist/title length.

    pub fn encode_external_id(r: &SoulseekRouting) -> String {
        let artist = r.artist.as_deref().unwrap_or("");
        let title = r.title.as_deref().unwrap_or("");
        let dur = r.duration.map(|d| d.to_string()).unwrap_or_default();
        format!(
            "yt|{}|{}|{}|{dur}",
            r.you_tube_id.as_deref().unwrap_or(""),
            b64_url_encode(artist),
            b64_url_encode(title)
        )
    }

    pub fn try_decode_external_id(external_id: Option<&str>) -> Option<SoulseekRouting> {
        let external_id = external_id.filter(|id| !dotnet::is_blank(id))?;
        let parts: Vec<&str> = external_id.split('|').collect();
        if parts.len() < 4 || parts[0] != "yt" {
            return None;
        }
        let duration = parts.get(4).and_then(|d| parse_int(d));
        Some(SoulseekRouting {
            you_tube_id: Some(parts[1].to_string()),
            artist: Some(b64_url_decode(parts[2])?),
            title: Some(b64_url_decode(parts[3])?),
            duration,
            ..Default::default()
        })
    }
}

impl Inner {
    fn lookup(&self, id: &str) -> Option<SharedRouting> {
        self.id_registry.lookup(id)
    }

    /// Reflect a catalog answer onto the song and its shared routing, so getSong stays
    /// consistent, and remember its length.
    fn apply_track_meta(&self, song: &mut Song, meta: &TrackMeta) {
        if let Some(d) = meta.duration.filter(|&d| d > 0) {
            song.duration = Some(d);
        }
        if let Some(album) = meta.album_title.as_deref().filter(|a| !dotnet::is_blank(a)) {
            song.album = album.to_string();
        }
    }

    fn reflect_on_routing(&self, id: &str, meta: &TrackMeta) {
        // Reflect onto the shared routing so getSong stays consistent.
        if let Some(routing) = self.lookup(id) {
            let mut r = routing.lock();
            if let Some(d) = meta.duration.filter(|&d| d > 0) {
                r.duration = Some(d);
            }
            if let Some(album) = meta.album_title.as_deref().filter(|a| !dotnet::is_blank(a)) {
                r.album = Some(album.to_string());
            }
        }
        self.id_registry
            .remember_length(id, meta.duration, LengthSource::Deezer);
    }

    /// Complete the rows below the first page from what is already known, then fetch the
    /// rest off the critical path so the next search for this query can complete them too.
    ///
    /// Reading the cache is free, so it happens inline and the rows it answers are real in
    /// THIS response. The fetch is not free, and awaiting it was worse than the bug it
    /// fixed: a 4s budget added 4s to every search to fill about ten rows, and a page only
    /// converged after five searches. Off the critical path the same work costs nothing and
    /// the second search answers all of it from cache.
    ///
    /// The warm still writes nothing back to a Song: what it finds goes on the routing
    /// instead, where the next response for the song reads it.
    ///
    /// Returns the rows the cache could not answer, for the caller to hand to that warm.
    fn enrich_remaining(&self, songs: Vec<&mut Song>) -> Vec<(String, String, String)> {
        let mut cold = Vec::new();
        for song in songs {
            let Some(meta) = self.deezer.cached_track(&song.artist, &song.title) else {
                cold.push(key_of(song));
                continue;
            };
            self.apply_track_meta(song, &meta);
            self.reflect_on_routing(&song.id, &meta);
        }
        cold
    }

    /// Look lengths up off the request, one song at a time, and store what is found on the
    /// registry. Order: Deezer, then Last.fm's track.getInfo, then a YouTube video's length
    /// inside the sane range. Songs that already have a metadata length are skipped.
    fn warm_lengths(self: &Arc<Self>, songs: impl IntoIterator<Item = (String, String, String)>) {
        let mut queued = Vec::new();
        for (id, artist, title) in songs {
            if id.is_empty() || dotnet::is_blank(&title) {
                continue;
            }
            match self.lookup(&id) {
                Some(routing) if !SongLength::has_metadata_length(&routing.lock()) => {}
                _ => continue,
            }
            if !self.length_lookups.lock().insert(id.clone()) {
                continue;
            }
            queued.push((id, artist, title));
        }
        if queued.is_empty() {
            return;
        }

        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            // Sequential on purpose: this has no deadline, and fanning out here is what
            // would eat the Deezer quota a live search needs. The year is skipped because
            // it costs a second request per album and no row shows it.
            for (id, artist, title) in queued {
                // Best-effort; the song keeps the placeholder until next time.
                let _ =
                    tokio::time::timeout(LENGTH_LOOKUP_TIMEOUT, this.look_up_length(&id, &artist, &title))
                        .await;
                this.length_lookups.lock().remove(&id);
            }
        });
        *self.last_length_warm.lock() = Some(task.map(|_| ()).boxed().shared());
    }

    async fn look_up_length(&self, id: &str, artist: &str, title: &str) {
        let meta = self.deezer.enrich_track(artist, title, false, true).await;
        if self
            .id_registry
            .remember_length(id, meta.and_then(|m| m.duration), LengthSource::Deezer)
        {
            return;
        }

        if let Some(last_fm) = self.last_fm.as_ref().filter(|l| l.has_api_key()) {
            let duration = last_fm.track_duration(artist, title).await;
            if self
                .id_registry
                .remember_length(id, duration, LengthSource::LastFm)
            {
                return;
            }
        }

        // Last, and only while the shim has room for background work: a video's length is
        // the weakest guess there is, and not worth making a play wait for.
        if let Some(routing) = self.lookup(id)
            && SongLength::shown(&routing.lock()).1 >= LengthSource::Video
        {
            return;
        }
        let Some(_permit) = bounded_wait(&self.prewarm_gate).await else {
            return;
        };
        // Length only. The video is not pinned for playback, so which video plays and
        // what a download is checked against both stay as they were.
        let hit = self.youtube.meta(&format!("{artist} {title}"), None, true).await;
        self.id_registry
            .remember_length(id, hit.and_then(|h| h.duration), LengthSource::Video);
    }

    /// The releases of the catalog artist an outside artist's name stands for, or none when no
    /// catalog artist has that name. The catalog id Octo already holds wins over a name search:
    /// it is the artist the user tapped in search, or the one an earlier visit settled on. On a
    /// library artist's page it must also share an album with the library, because two artists
    /// can share a name and the library says which one is meant. Without an id, only artists
    /// with this exact name count; of several, the one sharing the most albums with the
    /// library, else the one more people follow.
    ///
    /// The choice is kept for the next visit, and where depends on the page. The artist's
    /// routing is shared by everyone who reaches that name, from search, an album or a song,
    /// so an outside page keeps its choice there. A library artist's page keeps its own apart,
    /// under `page_key`: written onto the routing, the library's namesake
    /// became every listener's search row and outside page for the name.
    async fn find_artist_releases(
        &self,
        routing: SharedRouting,
        name: String,
        owned: HashSet<String>,
        page_key: String,
    ) -> Vec<AlbumHit> {
        let library_page = !owned.is_empty();
        let picked = if library_page {
            self.library_picks.lock().get(&page_key).cloned()
        } else {
            None
        };
        let known_id = picked.or_else(|| routing.lock().external_artist_id.clone());

        let mut known_releases: Option<Vec<AlbumHit>> = None;
        if let Some(known) = known_id.filter(|k| !k.is_empty()) {
            let releases = self.deezer.get_artist_albums(&known, &name).await;
            if !library_page {
                return releases;
            }
            if shared_count(&releases, &owned) > 0 {
                self.keep_library_pick(&page_key, &known);
                return releases;
            }
            known_releases = Some(releases);
        }

        let candidates = same_name(self.deezer.search_artists(&name, ARTIST_CANDIDATES).await, &name);
        let Some(mut pick) = most_followed(&candidates).cloned() else {
            return known_releases.unwrap_or_default();
        };
        let mut releases: Option<Vec<AlbumHit>> = None;
        if candidates.len() > 1 && !owned.is_empty() {
            let mut best = 0;
            // OrderByDescending(Fans): a stable sort, ties in the catalog's order.
            let mut by_fans: Vec<&ArtistHit> = candidates.iter().collect();
            by_fans.sort_by_key(|hit| std::cmp::Reverse(hit.fans));
            for candidate in by_fans.into_iter().take(ARTISTS_COMPARED) {
                let theirs = self.deezer.get_artist_albums(&candidate.deezer_id, &name).await;
                let shared = shared_count(&theirs, &owned);
                if shared <= best {
                    continue;
                }
                best = shared;
                pick = candidate.clone();
                releases = Some(theirs);
            }
        }
        let releases = match releases {
            Some(releases) => releases,
            None => self.deezer.get_artist_albums(&pick.deezer_id, &name).await,
        };

        if library_page {
            self.keep_library_pick(&page_key, &pick.deezer_id);
        } else {
            let changed = {
                let mut r = routing.lock();
                if r.external_artist_id.as_deref() != Some(pick.deezer_id.as_str()) {
                    r.external_artist_id = Some(pick.deezer_id.clone());
                    true
                } else {
                    false
                }
            };
            if changed {
                self.id_registry.register(routing.clone());
            }
        }
        releases
    }

    fn keep_library_pick(&self, page_key: &str, deezer_artist_id: &str) {
        let mut picks = self.library_picks.lock();
        if picks.len() >= LIBRARY_PICKS_KEPT && !picks.contains_key(page_key) {
            picks.clear();
        }
        picks.insert(page_key.to_string(), deezer_artist_id.to_string());
    }

    /// The catalog artist of this exact name an outside artist stands for: the one
    /// Octo already settled on, else the one more people follow. None when none has the name.
    /// The search is the one the album listing makes, so it costs nothing more.
    async fn find_catalog_artist(&self, routing: &SoulseekRouting) -> Option<ArtistHit> {
        let name = non_empty(routing.artist.as_deref())?;
        let candidates = same_name(self.deezer.search_artists(name, ARTIST_CANDIDATES).await, name);
        candidates
            .iter()
            .find(|hit| routing.external_artist_id.as_deref() == Some(hit.deezer_id.as_str()))
            .or_else(|| most_followed(&candidates))
            .cloned()
    }

    /// The listing carries no track counts. Each album's own record has one. Counts already
    /// known cost nothing; of the rest, the first few on the page are asked a few at a time, and
    /// the page waits a moment for them. What arrives in time is shown and the rest are kept for
    /// the next visit, so a long career fills in over a visit or two without flooding the
    /// catalog's quota. Without `fill`, only the counts already known.
    async fn fill_track_counts(&self, releases: &[AlbumHit], fill: bool) -> Vec<Option<i32>> {
        let counts = Arc::new(Mutex::new(vec![None; releases.len()]));
        let mut lookups = Vec::new();
        // Lookups still waiting when the page answers keep using it.
        let gate = Arc::new(Semaphore::new(TRACK_COUNTS_AT_ONCE));
        for (i, release) in releases.iter().enumerate() {
            if release.track_count > 0 {
                counts.lock()[i] = Some(release.track_count);
            } else if let Some(known) = self.deezer.try_known_track_count(&release.deezer_id) {
                counts.lock()[i] = known;
            } else if fill && lookups.len() < TRACK_COUNTS_PER_VISIT {
                let (deezer, gate, counts, id) = (
                    Arc::clone(&self.deezer),
                    Arc::clone(&gate),
                    Arc::clone(&counts),
                    release.deezer_id.clone(),
                );
                lookups.push(tokio::spawn(async move {
                    let Ok(_permit) = gate.acquire().await else { return };
                    let count = deezer.album_track_count(&id).await;
                    counts.lock()[i] = count;
                }));
            }
        }
        if !lookups.is_empty() {
            let _ = tokio::time::timeout(TRACK_COUNT_WAIT, futures::future::join_all(lookups)).await;
        }
        counts.lock().clone()
    }

    fn to_albums(&self, name: &str, walk: &ArtistWalk) -> Vec<Album> {
        walk.releases
            .iter()
            .zip(&walk.counts)
            .map(|(release, count)| {
                let album_id = self.id_registry.register(SoulseekRouting {
                    kind: RoutingKind::Album,
                    artist: Some(name.to_string()),
                    album: Some(release.title.clone()),
                    external_album_id: Some(release.deezer_id.clone()),
                    ..Default::default()
                });
                Album {
                    id: album_id.clone(),
                    title: release.title.clone(),
                    year: release.year,
                    song_count: Some(count.unwrap_or(release.track_count)),
                    cover_art_url: release.cover_url.clone(),
                    release_types: release_type_list(release.record_type.as_deref()),
                    is_local: false,
                    external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                    external_id: Some(album_id),
                    ..Default::default()
                }
            })
            .collect()
    }

    async fn artist_albums(
        self: &Arc<Self>,
        external_provider: &str,
        external_id: &str,
        library_album_titles: Option<&[String]>,
        fill_counts: bool,
    ) -> Vec<Album> {
        if !is_ours(external_provider) {
            return Vec::new();
        }
        let Some(routing) = self.lookup(external_id) else {
            return Vec::new();
        };
        let Some(name) = non_empty(routing.lock().artist.as_deref()).map(str::to_string) else {
            return Vec::new();
        };

        let owned: HashSet<String> = library_album_titles
            .unwrap_or_default()
            .iter()
            .map(|title| SongIdentity::key(title))
            .filter(|key| !key.is_empty())
            .collect();
        let mut sorted: Vec<&String> = owned.iter().collect();
        sorted.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let key = format!(
            "{external_id}|{}",
            sorted
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\u{1f}")
        );

        let releases = {
            let (this, routing, name, owned, key) = (
                Arc::clone(self),
                routing.clone(),
                name.clone(),
                owned.clone(),
                key.clone(),
            );
            move || {
                let walk = this.clone();
                this.release_walks.run(&key.clone(), async move {
                    walk.find_artist_releases(routing, name, owned, key).await
                })
            }
        };

        let walk = if fill_counts {
            let this = Arc::clone(self);
            self.count_walks
                .run(&key, async move {
                    let found = releases().await;
                    let counts = this.fill_track_counts(&found, true).await;
                    ArtistWalk {
                        releases: found,
                        counts,
                    }
                })
                .await
        } else {
            let found = releases().await;
            let counts = self.fill_track_counts(&found, false).await;
            ArtistWalk {
                releases: found,
                counts,
            }
        };

        // Each caller gets albums of its own: a library artist's page relinks them to itself.
        self.to_albums(&name, &walk)
    }

    /// An album the catalog cannot list still has the songs Octo showed under it: at least the
    /// one whose row named it. Listing those instead of nothing is what keeps a client that
    /// opens the album of the song it is playing (Tempo, #59) from finding it empty.
    fn list_songs_filed_under(
        &self,
        album: &mut Album,
        routing: &SoulseekRouting,
        placeholder: &str,
        artist_id: &str,
    ) {
        let mut filed: Vec<(String, SoulseekRouting)> = self
            .id_registry
            .songs_filed_under(
                routing.artist.as_deref(),
                Some(placeholder),
                SONGS_FILED_UNDER_LIMIT,
            )
            .into_iter()
            .map(|(id, shared)| (id, shared.snapshot()))
            .collect();
        // OrderBy(disc ?? 1).ThenBy(track ?? int.MaxValue), stable.
        filed.sort_by_key(|(_, song)| (song.disc_number.unwrap_or(1), song.track.unwrap_or(i32::MAX)));
        for (id, song) in filed {
            album.songs.push(Song {
                id: id.clone(),
                title: song.title.clone().unwrap_or_default(),
                artist: song.artist.clone().unwrap_or_default(),
                artist_id: Some(artist_id.to_string()),
                album: album.title.clone(),
                album_id: Some(album.id.clone()),
                duration: SongLength::shown(&song).0.or(song.duration),
                track: song.track,
                disc_number: song.disc_number,
                isrc: song.isrc.clone(),
                year: album.year,
                cover_art_url: album.cover_art_url.clone(),
                is_local: false,
                external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                external_id: Some(id),
                ..Default::default()
            });
        }
        if !album.songs.is_empty() {
            album.song_count = Some(album.songs.len() as i32);
        }
    }
}

/// What the length warm needs of a song: its id, artist and title.
fn key_of(song: &Song) -> (String, String, String) {
    (song.id.clone(), song.artist.clone(), song.title.clone())
}

#[async_trait]
impl IMusicMetadataService for SoulseekMetadataService {
    async fn search_songs(&self, query: &str, _limit: i32) -> Vec<Song> {
        if dotnet::is_blank(query) {
            return Vec::new();
        }
        let (query_artist, query_title) = parse_query(query);
        self.search_songs_by_artist_title(&query_artist, query_title.as_deref().unwrap_or(query), 1, None)
            .await
    }

    async fn search_songs_by_artist_title(
        &self,
        artist: &str,
        title: &str,
        _limit: i32,
        duration_seconds: Option<i32>,
    ) -> Vec<Song> {
        if dotnet::is_blank(artist) && dotnet::is_blank(title) {
            return Vec::new();
        }

        // INSTANT placeholder. We do NOT call YouTube here — at queue-build time we'd
        // rate-limit ourselves into oblivion (Arpeggio fans out 5-10 search3 calls
        // per radio session). YouTube resolution is deferred to /rest/stream where
        // it happens once per actual playback, sequentially as the user advances.
        let registry = &self.inner.id_registry;
        let external_id = registry.register(SoulseekRouting {
            // YouTubeId intentionally null — resolved lazily on play.
            artist: Some(artist.to_string()),
            title: Some(title.to_string()),
            duration: duration_seconds,
            ..Default::default()
        });

        debug!(
            "Placeholder song registered for '{artist} - {title}' (dur={duration_seconds:?}) -> id {external_id}"
        );

        // Every caller that hands in a length got it from Last.fm, or from a play's own
        // record. Stored by rank, so a Deezer length an earlier lookup found for this id
        // still wins, and that is the length this row goes out with.
        registry.remember_length(&external_id, duration_seconds, LengthSource::LastFm);
        let remembered = registry
            .lookup(&external_id)
            .and_then(|routing| SongLength::shown(&routing.lock()).0);

        // 180 is the fallback when we don't know the real duration — most songs
        // are 3-5 min so it's a less-bad guess than 0 (which would prevent
        // clients from rendering a scrub bar at all). The Octo app knows this
        // value and shows no length for it rather than a wrong one.
        let effective_duration = remembered.or(duration_seconds).unwrap_or(180);

        vec![Song {
            id: external_id.clone(),
            title: title.to_string(),
            artist: artist.to_string(),
            album: String::new(),
            duration: Some(effective_duration),
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id),
            ..Default::default()
        }]
    }

    async fn enrich_external_songs(&self, songs: &mut [Song]) {
        let inner = &self.inner;
        let mut external: Vec<&mut Song> = songs.iter_mut().filter(|s| !s.is_local).collect();
        let rest = external.split_off(external.len().min(SEARCH_ENRICH_LIMIT));
        let first_keys: Vec<(String, String, String)> = external.iter().map(|s| key_of(s)).collect();

        // First-page rows Deezer gave no length for. They join the background lookup below
        // rather than going out as 3:00 for good.
        let missed: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
        futures::stream::iter(external)
            .for_each_concurrent(8, |song| {
                let missed = &missed;
                async move {
                    let meta = inner
                        .deezer
                        .enrich_track(&song.artist, &song.title, true, false)
                        .await;
                    if !meta.as_ref().and_then(|m| m.duration).is_some_and(|d| d > 0) {
                        missed.lock().insert(song.id.clone());
                    }
                    let Some(meta) = meta else { return };
                    inner.apply_track_meta(song, &meta);
                    if let Some(y) = meta.year {
                        song.year = Some(y);
                    }
                    inner.reflect_on_routing(&song.id, &meta);
                }
            })
            .await;

        let rest: Vec<&mut Song> = rest
            .into_iter()
            .take(BACKGROUND_ENRICH_LIMIT - SEARCH_ENRICH_LIMIT)
            .collect();
        let cold = inner.enrich_remaining(rest);
        let missed = missed.into_inner();
        let first_missed = first_keys.into_iter().filter(|(id, _, _)| missed.contains(id));
        inner.warm_lengths(first_missed.chain(cold));
    }

    fn complete_song_lengths(&self, songs: &mut [Song]) {
        let inner = &self.inner;
        let mut cold = Vec::new();
        for song in songs.iter_mut() {
            if song.is_local || song.id.is_empty() {
                continue;
            }
            let Some(routing) = inner.lookup(&song.id) else {
                continue;
            };
            {
                let r = routing.lock();
                if r.kind != RoutingKind::Song || !r.has_artist_title() {
                    continue;
                }
                // Minting the song already applied whatever the registry remembered.
                if SongLength::has_metadata_length(&r) {
                    continue;
                }
            }

            // Free: a search for the same song may have asked Deezer already.
            if let Some(d) = inner
                .deezer
                .cached_track(&song.artist, &song.title)
                .and_then(|m| m.duration)
                .filter(|&d| d > 0)
            {
                song.duration = Some(d);
                inner
                    .id_registry
                    .remember_length(&song.id, Some(d), LengthSource::Deezer);
                continue;
            }
            cold.push(key_of(song));
        }
        inner.warm_lengths(cold.into_iter().take(STATION_LENGTH_WARM_LIMIT));
    }

    async fn resolve_top_durations(&self, songs: &mut [Song], background: bool) {
        let inner = &self.inner;
        let tasks = songs
            .iter_mut()
            .filter(|s| !s.is_local)
            .take(TOP_DURATION_RESOLVE_LIMIT)
            .map(|song| async move {
                // The background pass runs after the client has the results, so a play may already
                // have pinned a video for this song. It keeps it, and the shim is spared the lookup.
                if background
                    && inner
                        .lookup(&song.id)
                        .is_some_and(|r| r.lock().you_tube_id.as_deref().is_some_and(|y| !y.is_empty()))
                {
                    return;
                }
                let gate = if background {
                    &inner.background_duration_gate
                } else {
                    &inner.prewarm_gate
                };
                let Some(_permit) = bounded_wait(gate).await else {
                    return;
                };
                // Fast metadata-only lookup (flat search, no URL solve). Pass the
                // Deezer duration as a hint so it picks the closest-length canonical
                // video (not a long-form/compilation upload); playback reuses the
                // stored videoId, so the shown length matches the audio.
                let hit = inner
                    .youtube
                    .meta(
                        &format!("{} {}", song.artist, song.title),
                        song.duration,
                        background,
                    )
                    .await;
                let Some(hit) = hit.filter(|h| !h.video_id.is_empty()) else {
                    return;
                };
                let Some(d) = hit.duration.filter(|&d| d > 0) else {
                    return;
                };
                // Shown only inside the sane range. An hour-long upload is a mix or a
                // live set, and its length is no better than the one the row has.
                // Only the foreground pass may touch the Song. In the background the list is
                // already with the client and cached for the next page, being serialised as this
                // runs. getSong builds its answer from routing.Duration, so it still gets this length.
                if !background && let Some(shown) = SongLength::sane_video_length(Some(d)) {
                    song.duration = Some(shown);
                }
                let routing = inner.lookup(&song.id);
                if let Some(routing) = &routing {
                    let mut r = routing.lock();
                    // A play can pin a different video while this lookup runs. The next Range
                    // request has to get the same video, so the pinned one wins.
                    if background
                        && r.you_tube_id
                            .as_deref()
                            .is_some_and(|y| !y.is_empty() && y != hit.video_id)
                    {
                        return;
                    }
                    r.you_tube_id = Some(hit.video_id.clone()); // playback reuses this exact video
                    r.duration = Some(d);
                }
                inner
                    .id_registry
                    .remember_length(&song.id, Some(d), LengthSource::Video);
            });
        futures::future::join_all(tasks).await;
    }

    /// Fire-and-forget background prewarm: resolve the YouTube videoId (and via
    /// shim's automatic prefetch, the stream URL) for the first `top_n`
    /// placeholder songs from a search. Without this, Arpeggi's ~10s HTTP timeout
    /// fires while the cold yt-dlp ytsearch1: + yt-dlp -g chain is still running,
    /// the client cancels, and external songs never play.
    ///
    /// Only the top hits matter: search clients render in order and users almost
    /// never click past the first screen of results. Resolving 150 placeholders
    /// would saturate the shim's yt-dlp gate and waste work.
    async fn prewarm_you_tube_ids(&self, songs: &[Song], top_n: usize) {
        let ids: Vec<String> = songs
            .iter()
            .filter(|s| !s.id.is_empty())
            .map(|s| s.id.clone())
            .collect();
        self.prewarm_you_tube_ids_for_song_ids(&ids, top_n).await;
    }

    async fn prewarm_you_tube_ids_for_song_ids(&self, song_ids: &[String], top_n: usize) {
        let inner = &self.inner;
        // Skip ids whose YouTube resolution is already cached on the routing —
        // those are already warm and don't need a yt-dlp roundtrip. This is the
        // path used by the scrobble-driven sliding window: as the user advances
        // through a queue most upcoming items will still be cold, but if they
        // jump back to one we resolved earlier we don't burn shim cycles re-doing it.
        let targets: Vec<(String, SharedRouting)> = song_ids
            .iter()
            .filter(|id| !id.is_empty())
            .filter_map(|id| inner.lookup(id).map(|routing| (id.clone(), routing)))
            .filter(|(_, routing)| {
                let r = routing.lock();
                r.you_tube_id.as_deref().is_none_or(str::is_empty) && r.has_artist_title()
            })
            .take(top_n)
            .collect();
        if targets.is_empty() {
            return;
        }

        let tasks = targets.into_iter().map(|(id, routing)| async move {
            // Bounded wait, then drop. With a shared limiter an unbounded wait
            // lets a skip-happy user pile up prewarm tasks for songs they left
            // behind five tracks ago. Prewarm is best-effort by design, so its
            // queueing is best-effort too.
            let Some(_permit) = bounded_wait(&inner.prewarm_gate).await else {
                return;
            };
            let (query, duration) = {
                let r = routing.lock();
                if r.you_tube_id.as_deref().is_some_and(|y| !y.is_empty()) {
                    return;
                }
                (
                    format!(
                        "{} {}",
                        r.artist.as_deref().unwrap_or(""),
                        r.title.as_deref().unwrap_or("")
                    ),
                    r.duration,
                )
            };
            let hit = inner.youtube.search(&query, duration, true).await;
            if let Some(hit) = hit.filter(|h| !h.video_id.is_empty()) {
                {
                    let mut r = routing.lock();
                    r.you_tube_id = Some(hit.video_id.clone());
                    if let Some(d) = hit.duration {
                        r.duration = Some(d);
                    }
                }
                inner
                    .id_registry
                    .remember_length(&id, hit.duration, LengthSource::Video);
            }
        });
        futures::future::join_all(tasks).await;
    }

    /// Fire-and-forget background prewarm of cover art for the first `top_n`
    /// songs of a search, so a client that renders them a moment later finds the image
    /// already in [`CoverArtAggregator`]'s cache. Uses its own gate, separate from the
    /// shim-bound YouTube prewarm gate, and passes background: true through to the cover
    /// sources so this can never queue behind a live search or getCoverArt request.
    async fn prewarm_cover_art(&self, songs: &[Song], top_n: usize) {
        let inner = &self.inner;
        let targets: Vec<&Song> = songs.iter().filter(|s| !s.is_local).take(top_n).collect();
        if targets.is_empty() {
            return;
        }
        let tasks = targets.into_iter().map(|song| async move {
            let Some(_permit) = bounded_wait(&inner.cover_art_prewarm_gate).await else {
                return;
            };
            let routing = match inner.lookup(&song.id) {
                Some(routing) => routing.snapshot(),
                None => SoulseekRouting {
                    kind: RoutingKind::Song,
                    artist: Some(song.artist.clone()),
                    title: Some(song.title.clone()),
                    ..Default::default()
                },
            };
            inner.cover_art.get_cover(&routing, true).await;
        });
        futures::future::join_all(tasks).await;
    }

    async fn search_albums(&self, query: &str, limit: i32) -> Vec<Album> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let inner = &self.inner;
        let hits = inner.deezer.search_albums(query, limit, false).await;
        let mut albums = Vec::with_capacity(hits.len());

        for hit in hits {
            // The registry id is the external id everywhere: getAlbum, getCoverArt and
            // star all round-trip through it. The Deezer id rides along on the routing
            // so album detail can fetch the exact tracklist without a name lookup.
            let album_id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some(hit.artist.clone()),
                album: Some(hit.title.clone()),
                external_album_id: Some(hit.deezer_id.clone()),
                ..Default::default()
            });
            let artist_id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(hit.artist.clone()),
                ..Default::default()
            });

            albums.push(Album {
                id: album_id.clone(),
                title: hit.title.clone(),
                artist: hit.artist.clone(),
                artist_id: Some(artist_id),
                year: hit.year,
                song_count: Some(hit.track_count),
                cover_art_url: hit.cover_url.clone(),
                release_types: release_type_list(hit.record_type.as_deref()),
                is_local: false,
                external_provider: Some(Self::PROVIDER_NAME.to_string()),
                external_id: Some(album_id),
                ..Default::default()
            });
        }

        albums
    }

    async fn search_artists(&self, query: &str, limit: i32) -> Vec<Artist> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let inner = &self.inner;
        let hits = inner.deezer.search_artists(query, limit).await;

        // Two catalog artists of one name get one id, so they are one row: two rows opening
        // the same page would only confuse. The row is the artist Octo already settled on for
        // that name, else the one more people follow. (GroupBy keeps first-appearance order.)
        let mut groups: Vec<(String, Vec<ArtistHit>)> = Vec::new();
        for hit in hits {
            match groups.iter_mut().find(|(name, _)| *name == hit.name) {
                Some((_, group)) => group.push(hit),
                None => groups.push((hit.name.clone(), vec![hit])),
            }
        }

        let mut artists = Vec::with_capacity(groups.len());
        for (name, same) in groups {
            // Same registry id an album row mints for its artist, because the seed is the
            // artist name alone. So an artist found here and the same artist reached from
            // an album are one entity, and getArtist answers for both.
            let id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some(name),
                ..Default::default()
            });
            let routing = inner.lookup(&id);
            let settled = routing.as_ref().and_then(|r| r.lock().external_artist_id.clone());
            let hit = same
                .iter()
                .find(|h| settled.as_deref() == Some(h.deezer_id.as_str()))
                .or_else(|| most_followed(&same))
                .expect("a group is never empty")
                .clone();
            // Remembered only when nothing is yet: an earlier search or visit to the artist's
            // page already settled which artist of the name this is. An album never settles
            // it: the name's entry is everyone's, and the first album to show a little-known
            // namesake would have decided the name for all of them.
            if let Some(routing) = routing {
                let unsettled = {
                    let mut r = routing.lock();
                    let unsettled = r.external_artist_id.is_none();
                    if unsettled {
                        r.external_artist_id = Some(hit.deezer_id.clone());
                    }
                    unsettled
                };
                if unsettled {
                    inner.id_registry.register(routing);
                }
            }

            artists.push(Artist {
                id: id.clone(),
                name: hit.name.clone(),
                image_url: hit.picture_url.clone(),
                album_count: Some(hit.album_count),
                is_local: false,
                external_provider: Some(Self::PROVIDER_NAME.to_string()),
                external_id: Some(id),
            });
        }

        artists
    }

    async fn search_all(
        &self,
        query: &str,
        song_limit: i32,
        _album_limit: i32,
        _artist_limit: i32,
    ) -> SearchResult {
        let songs = self.search_songs(query, song_limit).await;
        SearchResult {
            songs,
            albums: Vec::new(),
            artists: Vec::new(),
        }
    }

    async fn get_song(&self, external_provider: &str, external_id: &str) -> Option<Song> {
        if !is_ours(external_provider) {
            return None;
        }

        let routing = match self.inner.lookup(external_id) {
            Some(routing) => routing.snapshot(),
            None => Self::try_decode_external_id(Some(external_id))?,
        };

        Some(Song {
            id: external_id.to_string(),
            title: routing.title.clone().unwrap_or_default(),
            artist: routing.artist.clone().unwrap_or_default(),
            // Carried from the routing so an album download tags the album the user
            // actually hearted rather than whatever Deezer guesses from artist+title,
            // and keeps its position so the album stays in order.
            album: routing.album.clone().unwrap_or_default(),
            track: routing.track,
            disc_number: routing.disc_number,
            total_tracks: routing.total_tracks,
            duration: routing.duration,
            isrc: routing.isrc.clone(),
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id.to_string()),
            ..Default::default()
        })
    }

    async fn get_album(&self, external_provider: &str, external_id: &str) -> Option<Album> {
        if !is_ours(external_provider) {
            return None;
        }
        let inner = &self.inner;
        let routing = inner.lookup(external_id)?.snapshot();

        let placeholder = routing
            .album
            .clone()
            .or_else(|| routing.title.clone())
            .unwrap_or_default();
        // Enrich by the track title (the placeholder "album" is the song title) so
        // Deezer returns the REAL album (e.g. "Creep" -> "Pablo Honey"). Degrades
        // to the placeholder name if Deezer misses or is unreachable.
        let artist_id = inner.id_registry.register(SoulseekRouting {
            kind: RoutingKind::Artist,
            artist: routing.artist.clone(),
            ..Default::default()
        });
        let artist = routing.artist.as_deref().unwrap_or("");

        // Resolve the Deezer album two ways. A search-derived routing already knows the
        // exact id. One minted from a song row does not, so recover the REAL album name
        // first (the placeholder is the song title, e.g. "Creep" -> "Pablo Honey") and
        // look the id up by name.
        let mut meta: Option<TrackMeta> = None;
        let mut deezer_album_id = routing.external_album_id.clone();
        if deezer_album_id.as_deref().is_none_or(str::is_empty) {
            let track = routing
                .album
                .as_deref()
                .or(routing.title.as_deref())
                .unwrap_or("");
            meta = inner.deezer.enrich_track(artist, track, true, false).await;
            let album_name = meta
                .as_ref()
                .and_then(|m| m.album_title.clone())
                .unwrap_or_else(|| placeholder.clone());
            deezer_album_id = inner.deezer.find_album_id(artist, &album_name).await;
        }

        let mut album = Album {
            id: external_id.to_string(),
            title: meta
                .as_ref()
                .and_then(|m| m.album_title.clone())
                .unwrap_or_else(|| placeholder.clone()),
            artist: artist.to_string(),
            artist_id: Some(artist_id.clone()),
            year: meta.as_ref().and_then(|m| m.year),
            cover_art_url: meta.as_ref().and_then(|m| m.album_cover_url.clone()),
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id.to_string()),
            ..Default::default()
        };

        let Some(deezer_album_id) = deezer_album_id.filter(|id| !id.is_empty()) else {
            // Logged rather than silent: this is what a user sees as an album that opens
            // with no tracks, and without a line here there is nothing to diagnose from.
            inner.list_songs_filed_under(&mut album, &routing, &placeholder, &artist_id);
            warn!(
                "getAlbum '{artist} - {placeholder}' ({external_id}): no Deezer album id resolved; listing the {} song(s) filed under it",
                album.songs.len()
            );
            return Some(album);
        };

        let lookup = inner.deezer.look_up_album_detail(&deezer_album_id).await;
        // An album with no resolvable tracklist must still render, so fall through with
        // whatever we already have rather than failing the request.
        let Some(detail) = lookup.detail else {
            // Deezer only failed to answer this time: no songs are filed in, because a
            // partial list would be taken for the whole album by a client that caches what
            // it syncs. When Deezer answered that it has no such album, or no tracks for it,
            // the songs filed under it are all there is to list.
            let stand_in = lookup.answer != AlbumAnswer::Unavailable;
            if stand_in {
                inner.list_songs_filed_under(&mut album, &routing, &placeholder, &artist_id);
            }
            let outcome = if stand_in {
                format!("listing the {} song(s) filed under it", album.songs.len())
            } else {
                "returning album without a tracklist".to_string()
            };
            warn!(
                "getAlbum '{artist} - {placeholder}' ({external_id}): Deezer album {deezer_album_id} returned no usable detail ({:?}; see the deezer warning above for why); {outcome}",
                lookup.answer
            );
            return Some(album);
        };

        album.title = detail.title.clone();
        album.year = detail.year.or(album.year);
        album.genre = detail.genre.clone();
        album.cover_art_url = detail.cover_url.clone().or(album.cover_art_url);
        album.release_types = release_type_list(detail.record_type.as_deref());
        // Defence in depth: the Deezer layer no longer returns a tracklist-less album,
        // but if one ever gets through, reporting zero is worse than saying nothing.
        if !detail.tracks.is_empty() {
            album.song_count = Some(detail.tracks.len() as i32);
        }
        if !dotnet::is_blank(&detail.artist) {
            album.artist = detail.artist.clone();
        }
        // Deezer says the album has no tracks at all: the songs filed under it stand in.
        if detail.tracks.is_empty() {
            inner.list_songs_filed_under(&mut album, &routing, &placeholder, &artist_id);
        }

        for track in &detail.tracks {
            // Album is carried on the ROUTING as well as the Song. The download path
            // re-resolves each track by id through GetSongAsync, and without this the
            // tagger re-derives the album from artist+title alone, which for a well
            // known single often lands on a greatest-hits record instead of this one.
            let track_id = inner.id_registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(track.artist.clone()),
                title: Some(track.title.clone()),
                album: Some(detail.title.clone()),
                duration: track.duration,
                track: track.track_position,
                disc_number: track.disc_number,
                total_tracks: Some(detail.tracks.len() as i32),
                isrc: track.isrc.clone(),
                ..Default::default()
            });

            album.songs.push(Song {
                id: track_id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                artist_id: Some(artist_id.clone()),
                album: detail.title.clone(),
                album_id: Some(external_id.to_string()),
                duration: track.duration,
                track: track.track_position,
                disc_number: track.disc_number,
                isrc: track.isrc.clone(),
                year: detail.year,
                genre: detail.genre.clone(),
                cover_art_url: detail.cover_url.clone(),
                cover_art_url_large: detail.cover_url.clone(),
                album_artist: Some(detail.artist.clone()),
                label: detail.label.clone(),
                is_local: false,
                external_provider: Some(Self::PROVIDER_NAME.to_string()),
                external_id: Some(track_id),
                ..Default::default()
            });
        }

        Some(album)
    }

    async fn get_artist(&self, external_provider: &str, external_id: &str) -> Option<Artist> {
        if !is_ours(external_provider) {
            return None;
        }
        let inner = &self.inner;
        let routing = inner.lookup(external_id)?.snapshot();

        // The name and picture of the catalog artist this page lists, as the album listing
        // settles it: the id already held, else the one of this exact name more people
        // follow. The first search hit can be a bigger act whose name contains this one.
        let hit = inner.find_catalog_artist(&routing).await;
        let meta = match hit {
            None => {
                inner
                    .deezer
                    .enrich_artist(routing.artist.as_deref().unwrap_or(""))
                    .await
            }
            Some(_) => None,
        };
        Some(Artist {
            id: external_id.to_string(),
            name: hit
                .as_ref()
                .map(|h| h.name.clone())
                .or_else(|| meta.as_ref().and_then(|m| m.name.clone()))
                .or_else(|| routing.artist.clone())
                .unwrap_or_default(),
            image_url: hit
                .as_ref()
                .and_then(|h| h.picture_url.clone())
                .or_else(|| meta.as_ref().and_then(|m| m.image_url.clone())),
            is_local: false,
            external_provider: Some(Self::PROVIDER_NAME.to_string()),
            external_id: Some(external_id.to_string()),
            ..Default::default()
        })
    }

    /// An outside artist's releases, for their page and for filling out a library artist's
    /// page. Each album is registered the way album search registers one, so it opens, plays
    /// and stars like any other outside album. The artist name and id are left for the caller:
    /// a library artist's page links its albums back to the library artist.
    async fn get_artist_albums(&self, external_provider: &str, external_id: &str) -> Vec<Album> {
        self.get_artist_albums_for_library(external_provider, external_id, None)
            .await
    }

    /// The same, for a library artist's page: the library's album titles say which of two
    /// artists of one name the page is about.
    async fn get_artist_albums_for_library(
        &self,
        external_provider: &str,
        external_id: &str,
        library_album_titles: Option<&[String]>,
    ) -> Vec<Album> {
        self.inner
            .artist_albums(external_provider, external_id, library_album_titles, true)
            .await
    }

    /// The same list with only the track counts already known, for the counts an artist's own
    /// record shows. The page asks for its album list at the same moment, and that request
    /// asks the catalog for the missing counts; asking again here doubled the traffic.
    async fn get_artist_albums_known_counts(&self, external_provider: &str, external_id: &str) -> Vec<Album> {
        self.inner
            .artist_albums(external_provider, external_id, None, false)
            .await
    }

    async fn search_playlists(&self, _query: &str, _limit: i32) -> Vec<ExternalPlaylist> {
        Vec::new()
    }

    async fn get_playlist(&self, _external_provider: &str, _external_id: &str) -> Option<ExternalPlaylist> {
        None
    }

    async fn get_playlist_tracks(&self, _external_provider: &str, _external_id: &str) -> Vec<Song> {
        Vec::new()
    }
}

#[cfg(test)]
#[path = "soulseek_metadata_service_tests.rs"]
mod tests;
