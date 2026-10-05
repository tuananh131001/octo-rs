//! Port of `Services/Common/ExternalSearchService.cs`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet::{self, ordinal_ignore_case_key};
use octo_core::common::{SongIdentity, SupersedableBuildCoordinator};
use octo_core::last_fm::last_fm_search_cleanup::LastFmSearchCleanup;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::SettingsStore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::LastFmService;

/// Rows built per query, regardless of how many the caller wants.
///
/// It has to be a constant rather than the caller's target, or single-flight is
/// unsound: a client asking for 8 rows could win the race and hand 8 rows to a caller
/// that asked for 150. 60 is the number because that is where enrichment stops
/// (BackgroundEnrichLimit), so rows past it would ship as bare placeholders carrying a
/// fallback duration, and because the Navidrome-native search path already caps here.
pub const BUILD_SIZE: i32 = 60;

// track.search returns at most 50 rows, so padding whenever the list was short of
// BuildSize made the second, sequential Last.fm call on every search.
const THIN_SEARCH_THRESHOLD: usize = 20;

/// Deadline for one build. Last.fm has no configured HTTP timeout of its own, so
/// without this a single hung call would pin the query for every joined caller.
const BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for one album build. Deezer's own client timeout already bounds
/// the call inside it; this is the ceiling on the whole build.
const ALBUM_BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the external (discovery) half of a search, once per query.
///
/// Every caller for the same query joins one execution and receives the same list. That
/// is not an optimisation, it is what keeps the search3 fix from re-creating issue #8.
/// Clients routinely fire several search calls for one typed query, and registry ids are
/// deterministic, so those calls resolve to the *same* SoulseekRouting objects and would
/// each run the enrichment pipeline over them concurrently — three writers to a shared
/// int? duration, and three times the Deezer fan-out against a budget that is already the
/// tightest thing in the system.
///
/// The returned list is FROZEN (an `Arc` nobody can write through). Callers slice it and
/// serialise it, concurrently, without copying. Both Subsonic serialisers and the native one
/// only read, and the star/download paths rebuild a Song from the registry rather than from a
/// search result. Any future "top up the enrichment because this caller wanted more rows"
/// belongs inside the build, not after it.
pub struct ExternalSearchService {
    // Amperfy (and most Subsonic clients' type-ahead) fires one search3 call per
    // keystroke, uncancelled. Plain SingleFlight only collapses two callers asking for
    // the SAME query; it does nothing for the sequence "cage", "cage t", "cage the",
    // where each keystroke is a distinct key racing the same rate-limited lane as the
    // query the user actually meant. SupersedableBuildCoordinator adds prefix-based
    // cancellation on top of that collapsing.
    song_builds: SupersedableBuildCoordinator<Arc<Vec<Song>>>,
    album_builds: SupersedableBuildCoordinator<Arc<Vec<Album>>>,
    metadata: Arc<dyn IMusicMetadataService>,
    last_fm: Option<Arc<LastFmService>>,
    /// `IOptionsMonitor<SubsonicSettings>?`: `WaitForSearchDurations`, read at every build.
    settings: Option<Arc<SettingsStore>>,
}

impl ExternalSearchService {
    pub const BUILD_SIZE: i32 = BUILD_SIZE;

    pub fn new(
        metadata: Arc<dyn IMusicMetadataService>,
        last_fm: Option<Arc<LastFmService>>,
        settings: Option<Arc<SettingsStore>>,
    ) -> Self {
        ExternalSearchService {
            song_builds: SupersedableBuildCoordinator::new(),
            album_builds: SupersedableBuildCoordinator::new(),
            metadata,
            last_fm,
            settings,
        }
    }

    /// Up to [`BUILD_SIZE`] enriched external songs for this query. Callers take
    /// the prefix they need; the list is shared and must not be mutated.
    pub async fn get(&self, query: &str) -> Arc<Vec<Song>> {
        if dotnet::is_blank(query) {
            return Arc::new(Vec::new());
        }
        // Only the key, not the radio switch. Discovery in the search bar is a different
        // feature from radio, and gating it on EnableRadio made turning radio off empty
        // the search results too.
        let Some(last_fm) = self.last_fm.clone().filter(|l| l.has_api_key()) else {
            return Arc::new(Vec::new());
        };

        let metadata = self.metadata.clone();
        let wait_for_durations = self
            .settings
            .as_ref()
            .is_none_or(|s| s.current().subsonic.wait_for_search_durations);
        let query_text = query.to_string();
        self.song_builds
            .run(
                query.trim(),
                move |token: CancellationToken| async move {
                    tokio::select! {
                        built = build(query_text, last_fm, metadata, wait_for_durations) => Ok(Arc::new(built)),
                        () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
                    }
                },
                BUILD_TIMEOUT,
                Arc::new(Vec::new()),
                // Discovery is an addition to search, never a precondition for it. The
                // steps inside the build are individually best-effort, but the deadline is
                // not, and a timeout must not take local results down with it.
                |q: &str, e: &anyhow::Error| debug!("external search '{q}' failed: {e}"),
            )
            .await
    }

    /// Up to `limit` external albums for this query, via the same prefix-supersession as
    /// [`ExternalSearchService::get`]. Needs no Last.fm key: Deezer's album catalog is keyless.
    pub async fn get_albums(&self, query: &str, limit: i32) -> Arc<Vec<Album>> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Arc::new(Vec::new());
        }
        let metadata = self.metadata.clone();
        let query_text = query.to_string();
        self.album_builds
            .run(
                query,
                move |token: CancellationToken| async move {
                    tokio::select! {
                        albums = metadata.search_albums(&query_text, limit) => Ok(Arc::new(albums)),
                        () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
                    }
                },
                ALBUM_BUILD_TIMEOUT,
                Arc::new(Vec::new()),
                |q: &str, e: &anyhow::Error| debug!("external album search '{q}' failed: {e}"),
            )
            .await
    }
}

/// Fans out to Last.fm, then fills in the metadata a client needs to render and play
/// the rows. Order:
///   1. track.search hits (best fuzzy matches for the query as typed)
///   2. canonical artist's top tracks (in case (1) was thin — common for
///      single-word artist queries)
///
/// Deduped by `SongIdentity::match_key`, so the same track cannot appear twice however its
/// artist and title are written.
async fn build(
    query: String,
    last_fm: Arc<LastFmService>,
    metadata: Arc<dyn IMusicMetadataService>,
    wait_for_durations: bool,
) -> Vec<Song> {
    let build_size = BUILD_SIZE as usize;
    // HashSet<string>(OrdinalIgnoreCase) over the match keys.
    let mut seen = HashSet::new();
    let mut collected: Vec<(String, String)> = Vec::new();

    // Last.fm's search carries rows from mislabelled scrobbles; put their names right first,
    // or they reach the results, the player and each listener's Last.fm as they are.
    let tracks = LastFmSearchCleanup::clean(&last_fm.search_tracks(&query, 50.min(BUILD_SIZE * 2)).await);
    for t in &tracks {
        let key = SongIdentity::match_key(&t.artist, &t.title);
        if seen.insert(ordinal_ignore_case_key(&key)) {
            collected.push((t.artist.clone(), t.title.clone()));
        }
        if collected.len() >= build_size {
            break;
        }
    }

    if collected.len() < THIN_SEARCH_THRESHOLD {
        // Use the first track-search hit's artist as the canonical anchor
        // for top-tracks padding. Falls back to the raw query string when
        // track.search came back empty.
        let anchor = tracks.first().map_or(query.as_str(), |t| t.artist.as_str());
        let top_tracks = last_fm.get_artist_top_tracks(anchor, BUILD_SIZE * 2).await;
        for t in &top_tracks {
            let key = SongIdentity::match_key(&t.artist, &t.title);
            if seen.insert(ordinal_ignore_case_key(&key)) {
                collected.push((t.artist.clone(), t.title.clone()));
            }
            if collected.len() >= build_size {
                break;
            }
        }
    }

    let mut songs = Vec::with_capacity(collected.len());
    for (artist, title) in &collected {
        if let Some(hit) = metadata
            .search_songs_by_artist_title(artist, title, 1, None)
            .await
            .into_iter()
            .next()
        {
            songs.push(hit);
        }
    }
    info!("External search '{query}' -> {} placeholder songs", songs.len());

    // Album/art/year from Deezer (fast), then the ACCURATE duration for the top of the
    // list from the real YouTube video (so the scrub bar matches the audio and the
    // client advances correctly). Bounded + cached.
    metadata.enrich_external_songs(&mut songs).await;
    if wait_for_durations {
        metadata.resolve_top_durations(&mut songs, false).await;
    }

    // Fire-and-forget: pre-resolve YouTube videoIds for the top hits so the first
    // /rest/stream click doesn't pay the cold yt-dlp double-call cost (ytsearch1: + -g,
    // 6-16s combined). Arpeggi cancels at ~10s and falls back to a local song; without
    // this, external playback is unreachable from that client. 12 is about what fits on
    // the first page of search results.
    //
    // It runs LAST on purpose. It used to run before enrichment, where it wrote a
    // videoId chosen with no duration hint while ResolveTopDurationsAsync was choosing
    // a different one using the Deezer duration — so for the top rows the two raced and
    // the loser could leave a song advertising the length of a video that would not be
    // the one played.
    //
    // The background work gets its own copy of the list: the songs this build returns stay
    // frozen, and in background mode the durations pass writes only the routing.
    let background = songs.clone();
    let prewarm = metadata.clone();
    if wait_for_durations {
        tokio::spawn(async move { prewarm.prewarm_you_tube_ids(&background, 12).await });
    } else {
        tokio::spawn(resolve_durations_then_prewarm(prewarm, background));
    }

    // Same reasoning, for cover art: a client renders the first screen of results a
    // moment after this returns, and without a prewarm each row's getCoverArt call
    // pays for the Deezer/iTunes/Last.fm chain itself, one row at a time. 24 covers
    // more than a screenful so scrolling a little still finds a warm cache.
    let covers = songs.clone();
    tokio::spawn(async move { metadata.prewarm_cover_art(&covers, 24).await });

    songs
}

/// The durations pass search did not wait for, then the videoId prewarm, in that order
/// for the reason given in `build`. It runs in background mode, so the songs this
/// build returned stay frozen.
async fn resolve_durations_then_prewarm(metadata: Arc<dyn IMusicMetadataService>, mut songs: Vec<Song>) {
    metadata.resolve_top_durations(&mut songs, true).await;
    metadata.prewarm_you_tube_ids(&songs, 12).await;
}

#[cfg(test)]
#[path = "external_search_service_tests.rs"]
mod tests;
