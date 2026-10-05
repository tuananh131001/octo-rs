//! Port of `Services/Metadata/DeezerMetadataService.cs`. The records it answers with are in
//! `octo_core::metadata::deezer_metadata_service`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{self, escape_data_string, is_blank};
use octo_core::json::element::{
    ElementResult, array_length, enumerate_array, get_double, get_int32, get_int64, str_prop,
    try_get_property,
};
use octo_core::metadata::accept_language_header;
pub use octo_core::metadata::deezer_metadata_service::{
    AlbumAnswer, AlbumDetail, AlbumHit, AlbumLookup, AlbumTrack, ArtistHit, ArtistMeta, CatalogCandidates,
    FullTrackMeta, TrackMeta, release_rank, release_types,
};
use octo_core::settings::SettingsStore;
use octo_core::tagging::CatalogLookup;
use serde_json::Value;
use tracing::{debug, info, warn};

use super::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::framework::{InFlight, MemoryCache};

const MAX_CACHE: u64 = 4096;

/// The only Deezer error code meaning "this genuinely does not exist". Everything
/// else, including quota (code 4) and any code we do not recognise, is treated as
/// transient. Caching an error we do not understand is exactly how one throttled
/// call turned into an album that reported zero tracks for the life of the process.
const DEFINITIVE_ERROR_CODE: i32 = 800;

/// Good answers are stable, so this only needs to be short enough that a
/// long-lived container eventually picks up catalog corrections.
const POSITIVE_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// "Deezer answered and this does not exist." Still expires, because the
/// catalog gains releases.
const NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);

/// A tracklist we know is incomplete. Usable now, refetched soon.
const PARTIAL_TTL: Duration = Duration::from_secs(10 * 60);

/// How many searches one track lookup may make: the song as asked, then written
/// another way, then the title alone. Each is one request against Deezer's shared budget,
/// and only a miss makes the next.
const TRACK_SEARCHES: usize = 3;

/// How many hits to weigh before giving up. A plain query is fuzzier than a
/// qualified one was, so it puts covers, karaoke and live takes next to the real
/// recording and the first row is not reliably the right one.
const MATCH_CANDIDATES: i32 = 5;

/// How many releases an artist's page asks for: the catalog's page size, enough for
/// all but the longest careers.
const ARTIST_ALBUMS_LIMIT: i32 = 100;

/// What the cache holds, one variant per kind of answer. The C# cache held `Entry<T>`
/// wrappers, so a cached null was distinguishable from a cache miss; `Option` payloads do that
/// here. Keys carry a prefix per kind, so a key never meets another kind's value.
#[derive(Clone)]
enum Cached {
    Track(Option<TrackMeta>),
    Full(Option<FullTrackMeta>),
    Candidates(Arc<CatalogCandidates>),
    Artist(Option<ArtistMeta>),
    ArtistHits(Vec<ArtistHit>),
    AlbumHits(Vec<AlbumHit>),
    Number(Option<i32>),
    Id(Option<String>),
    Text(String),
    Album(AlbumLookup),
}

/// Result of one Deezer call. Deezer answers HTTP 200 even when it is refusing the
/// request, so "we parsed a document" is not the same as "the call succeeded", and
/// callers must never cache anything derived from a transient failure.
struct DeezerResponse {
    doc: Option<Value>,

    /// Failed in a way that may succeed later. Nothing about this call
    /// may be written to a cache.
    transient: bool,
}

impl DeezerResponse {
    fn transient() -> Self {
        Self {
            doc: None,
            transient: true,
        }
    }
}

/// Enriches external (YouTube-resolved) tracks with real album/artist metadata
/// from Deezer's public API. Keyless and no ARL — the ARL that expires on the
/// music bot is only for Deezer AUDIO; metadata endpoints are open.
///
/// Everything here is best-effort and cached: a Deezer outage, throttle, or miss
/// returns null, and callers fall back to a synthetic entity. Nothing on this
/// path ever blocks or fails playback.
///
/// Cheap to clone: the clones share one cache.
#[derive(Clone)]
pub struct DeezerMetadataService {
    inner: Arc<Inner>,
}

struct Inner {
    http: Arc<DeezerRateLimitHandler>,
    settings: Arc<SettingsStore>,
    base: String,

    // Owned rather than shared: metadata records are tens of bytes and
    // cover-art blobs are hundreds of kilobytes, so a single shared SizeLimit cannot
    // be right for both. Every entry counts as 1, so the limit is an entry count.
    cache: MemoryCache<Cached>,

    // Single-flight the album-year fetch: many tracks in one search share an album
    // (a whole album's tracks), so concurrent lookups collapse onto one HTTP call.
    // None is a fetch that threw.
    album_year_tasks: InFlight<Option<(Option<i32>, bool)>>,

    /// Requests on their way to the catalog, by cache key. The cache only answers
    /// once a request is back, so two callers asking the same thing at once each asked.
    in_flight_artists: InFlight<Vec<ArtistHit>>,
    in_flight_counts: InFlight<Option<i32>>,
}

/// Flow out of a C# `try` block that could `return` early.
enum Flow<T> {
    Return(T),
    Done,
}

impl DeezerMetadataService {
    pub const BASE: &'static str = "https://api.deezer.com";

    pub fn new(http: Arc<DeezerRateLimitHandler>, settings: Arc<SettingsStore>) -> Self {
        Self::with_base_url(http, settings, Self::BASE)
    }

    /// A service that asks another host than api.deezer.com: a test's mock server.
    pub fn with_base_url(
        http: Arc<DeezerRateLimitHandler>,
        settings: Arc<SettingsStore>,
        base: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                settings,
                base: base.into().trim_end_matches('/').to_string(),
                cache: MemoryCache::new(MAX_CACHE),
                album_year_tasks: InFlight::new(),
                in_flight_artists: InFlight::new(),
                in_flight_counts: InFlight::new(),
            }),
        }
    }

    fn base(&self) -> &str {
        &self.inner.base
    }

    fn get_cached(&self, key: &str) -> Option<Cached> {
        self.inner.cache.get(key)
    }

    fn put(&self, key: impl Into<String>, value: Cached, ttl: Duration) {
        self.inner.cache.set(key, value, 1, ttl);
    }

    /// Drop every cached answer. Exposed so a poisoned cache can be cleared
    /// without restarting the container.
    pub fn clear_caches(&self) {
        self.inner.cache.clear();
        self.inner.album_year_tasks.clear();
        info!("deezer metadata caches cleared");
    }

    fn track_key(artist: &str, title: &str) -> String {
        dotnet::to_lower_invariant(&format!("t|{artist}|{title}"))
    }

    /// What is already known about a track, or None when nothing is. Never makes a
    /// request, so a caller on a latency budget can complete the rows it knows without
    /// paying for the ones it does not. Shares the track key with [`Self::enrich_track`],
    /// because a lookup keyed differently from the write would silently never hit.
    pub fn cached_track(&self, artist: &str, title: &str) -> Option<TrackMeta> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        match self.get_cached(&Self::track_key(artist, title)) {
            Some(Cached::Track(meta)) => meta,
            _ => None,
        }
    }

    /// Resolve "artist + title" to the real album + artist (name, art, year).
    /// Pass include_year=false to skip the extra album-detail call (bulk enrichment
    /// wants duration + album fast; the year is fetched lazily by the album view).
    pub async fn enrich_track(
        &self,
        artist: &str,
        title: &str,
        include_year: bool,
        background: bool,
    ) -> Option<TrackMeta> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        let key = Self::track_key(artist, title);
        if let Some(Cached::Track(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut meta = None;
        // Set when the year alone could not be resolved. The rest of the record is still
        // good and is returned; it just must not be remembered, or a throttle blip would
        // be cached as "this track has no year" for the life of the entry.
        let mut year_unresolved = false;
        let attempt = async {
            let (transient, found) = self.find_track(artist, title, background).await?;
            if transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some(t) = found {
                let (mut alb_title, mut cover, mut art_name, mut art_img) = (None, None, None, None);
                let mut alb_id = 0;
                if let Some(alb) = try_get_property(&t, "album")? {
                    alb_title = str_prop(alb, "title")?;
                    cover = str_prop(alb, "cover_xl")?.or(str_prop(alb, "cover_medium")?);
                    if let Some(aid @ Value::Number(_)) = try_get_property(alb, "id")? {
                        alb_id = get_int64(aid)?;
                    }
                }
                if let Some(art) = try_get_property(&t, "artist")? {
                    art_name = str_prop(art, "name")?;
                    art_img = str_prop(art, "picture_xl")?.or(str_prop(art, "picture_medium")?);
                }
                let duration = match try_get_property(&t, "duration")? {
                    Some(du @ Value::Number(_)) => Some(get_int32(du)?),
                    _ => None,
                };
                let mut year = None;
                if include_year && alb_id > 0 {
                    let (y, year_transient) = self.album_year(alb_id).await?;
                    // Degrade to "no year", never to "no track". Returning null here threw
                    // away an album title and duration the search had already fetched, so a
                    // throttled year left the song with no length at all.
                    year = y;
                    year_unresolved = year_transient;
                }
                meta = Some(TrackMeta {
                    album_title: alb_title.map(str::to_string),
                    album_cover_url: cover.map(str::to_string),
                    year,
                    duration,
                    artist_name: art_name.map(str::to_string),
                    artist_image_url: art_img.map(str::to_string),
                });
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer enrich track '{artist} - {title}' failed: {e}"),
        }

        // Usable now, refetched next time, so the year gets another chance.
        if year_unresolved {
            return meta;
        }

        let ttl = if meta.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::Track(meta.clone()), ttl);
        meta
    }

    /// Full track metadata for tagging a downloaded file: one track search (album,
    /// cover_xl, artist, duration, track_position, disk_number, isrc) plus one album
    /// detail call (release year, genre, total tracks, label). Cached; best-effort.
    pub async fn enrich_track_full(&self, artist: &str, title: &str) -> Option<FullTrackMeta> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        let key = dotnet::to_lower_invariant(&format!("full|{artist}|{title}"));
        if let Some(Cached::Full(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut meta = None;
        // The album detail carries the year, genre and label. If only that call fails the
        // track itself is still worth having, so it is returned uncached rather than lost:
        // a tagger with an album title and cover art beats one with nothing.
        let mut detail_unresolved = false;
        let attempt = async {
            let (transient, found) = self.find_track(artist, title, false).await?;
            if transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some(t) = found {
                let (full, unresolved) = self.build_full_meta(&t).await?;
                meta = Some(full);
                detail_unresolved = unresolved;
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer full enrich '{artist} - {title}' failed: {e}"),
        }

        // Usable now, refetched next time, so the album detail gets another chance.
        if detail_unresolved {
            return meta;
        }

        let ttl = if meta.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::Full(meta.clone()), ttl);
        meta
    }

    /// The ranked hits for one song, not only the first: every hit of the first search that
    /// finds any, in the catalog's order, with the detail calls made for the best `max`. The
    /// chooser weighs them against the other sources. A throttled catalog answers nothing and
    /// says so, and nothing from that call is remembered.
    pub async fn enrich_track_candidates(
        &self,
        artist: &str,
        title: &str,
        max: i32,
    ) -> Arc<CatalogCandidates> {
        if is_blank(artist) && is_blank(title) {
            return Arc::new(CatalogCandidates::default());
        }
        let max = max.clamp(1, MATCH_CANDIDATES);
        let key = dotnet::to_lower_invariant(&format!("cands|{artist}|{title}|{max}"));
        if let Some(Cached::Candidates(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut hits = Vec::new();
        let mut detail_unresolved = false;
        let attempt = async {
            let (transient, found) = self.find_track_hits(artist, title).await?;
            if transient {
                return anyhow::Ok(Flow::Return(()));
            }
            for hit in found.iter().take(max as usize) {
                let (meta, unresolved) = self.build_full_meta(hit).await?;
                detail_unresolved |= unresolved;
                hits.push(meta);
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => {
                return Arc::new(CatalogCandidates {
                    hits: Vec::new(),
                    did_not_answer: true,
                });
            }
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer candidates '{artist} - {title}' failed: {e}"),
        }

        let empty = hits.is_empty();
        let answer = Arc::new(CatalogCandidates {
            hits,
            did_not_answer: false,
        });
        if !detail_unresolved {
            let ttl = if empty { NEGATIVE_TTL } else { POSITIVE_TTL };
            self.put(key, Cached::Candidates(Arc::clone(&answer)), ttl);
        }
        answer
    }

    /// One search hit made whole: the album's detail (year, genre, label, barcode,
    /// album artist, kind) and the track's own record (position, contributors, code, words,
    /// loudness). The second value says a detail call did not answer, so nothing is cached.
    async fn build_full_meta(&self, t: &Value) -> ElementResult<(FullTrackMeta, bool)> {
        let mut detail_unresolved = false;
        let (mut alb_title, mut cover, mut art_name) = (None, None, None);
        let mut isrc = str_prop(t, "isrc")?.map(str::to_string);
        let mut alb_id = 0;
        if let Some(alb) = try_get_property(t, "album")? {
            alb_title = str_prop(alb, "title")?.map(str::to_string);
            cover = str_prop(alb, "cover_xl")?
                .or(str_prop(alb, "cover_big")?)
                .or(str_prop(alb, "cover_medium")?)
                .map(str::to_string);
            if let Some(aid @ Value::Number(_)) = try_get_property(alb, "id")? {
                alb_id = get_int64(aid)?;
            }
        }
        if let Some(art) = try_get_property(t, "artist")? {
            art_name = str_prop(art, "name")?.map(str::to_string);
        }

        let (mut year, mut total_tracks) = (None, None);
        let (mut genre, mut label, mut release_date, mut album_artist, mut record_type, mut barcode) =
            (None, None, None, None, None, None);
        if alb_id > 0 {
            let ar = self
                .get_json(&format!("{}/album/{alb_id}", self.base()), false)
                .await;
            detail_unresolved = ar.transient;
            if let Some(root) = &ar.doc {
                record_type = str_prop(root, "record_type")?.map(str::to_string);
                if let Some(album_art @ Value::Object(_)) = try_get_property(root, "artist")? {
                    album_artist = str_prop(album_art, "name")?.map(str::to_string);
                }
                release_date = str_prop(root, "release_date")?.map(str::to_string);
                if let Some(yr) = release_date.as_deref().and_then(year_prefix) {
                    year = Some(yr);
                }
                total_tracks = int(root, "nb_tracks")?;
                label = str_prop(root, "label")?.map(str::to_string);
                barcode = str_prop(root, "upc")?.map(str::to_string);
                genre = first_genre(root)?;
            }
        }

        // The search hit carries neither the track's position nor anyone but the main
        // artist, so the track number was never written (#48) and a collaboration was one
        // artist (#49). The track's own record has both.
        let mut track_number = int(t, "track_position")?;
        let mut disc_number = int(t, "disk_number")?;
        let mut contributors = None;
        let mut explicit_lyrics = match try_get_property(t, "explicit_lyrics")? {
            Some(Value::Bool(flag)) => Some(*flag),
            _ => None,
        };
        let mut gain = None;
        let mut track_id = None;
        if let Some(tid @ Value::Number(_)) = try_get_property(t, "id")? {
            let id = get_int64(tid)?;
            track_id = Some(id.to_string());
            let tr = self.get_json(&format!("{}/track/{id}", self.base()), false).await;
            detail_unresolved |= tr.transient;
            if let Some(track) = &tr.doc {
                if track_number.is_none() {
                    track_number = int(track, "track_position")?;
                }
                if disc_number.is_none() {
                    disc_number = int(track, "disk_number")?;
                }
                if isrc.is_none() {
                    isrc = str_prop(track, "isrc")?.map(str::to_string);
                }
                if let Some(Value::Bool(flag)) = try_get_property(track, "explicit_lyrics")? {
                    explicit_lyrics = Some(*flag);
                }
                if let Some(gn @ Value::Number(_)) = try_get_property(track, "gain")? {
                    gain = Some(get_double(gn)?);
                }
                if let Some(people @ Value::Array(_)) = try_get_property(track, "contributors")? {
                    let mut names: Vec<String> = Vec::new();
                    for person in enumerate_array(people)? {
                        if !matches!(str_prop(person, "role")?, None | Some("Main") | Some("Featured")) {
                            continue;
                        }
                        let Some(name) = str_prop(person, "name")?.filter(|n| !is_blank(n)) else {
                            continue;
                        };
                        if !names.iter().any(|known| dotnet::eq_ignore_case(known, name)) {
                            names.push(name.to_string());
                        }
                    }
                    contributors = Some(names);
                }
            }
        }

        let meta = FullTrackMeta {
            album_title: alb_title,
            album_cover_url: cover,
            year,
            duration: int(t, "duration")?,
            artist_name: art_name,
            track_number,
            disc_number,
            isrc,
            total_tracks,
            genre,
            label,
            release_date,
            contributors,
            album_artist_name: album_artist,
            record_type,
            title: str_prop(t, "title")?.map(str::to_string),
            track_id,
            album_id: (alb_id > 0).then(|| alb_id.to_string()),
            barcode: barcode.filter(|b| !is_blank(b)),
            explicit_lyrics,
            catalog_gain: gain,
        };
        Ok((meta, detail_unresolved))
    }

    /// Resolve an artist name to its Deezer name + image.
    pub async fn enrich_artist(&self, artist: &str) -> Option<ArtistMeta> {
        if is_blank(artist) {
            return None;
        }
        let key = dotnet::to_lower_invariant(&format!("a|{artist}"));
        if let Some(Cached::Artist(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut meta = None;
        let attempt = async {
            let q = escape_data_string(artist);
            let r = self
                .get_json(&format!("{}/search/artist?q={q}&limit=1", self.base()), false)
                .await;
            if r.transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some(a) = first_data(r.doc.as_ref())? {
                meta = Some(ArtistMeta {
                    name: str_prop(a, "name")?.map(str::to_string),
                    image_url: str_prop(a, "picture_xl")?
                        .or(str_prop(a, "picture_medium")?)
                        .map(str::to_string),
                });
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer enrich artist '{artist}' failed: {e}"),
        }

        let ttl = if meta.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::Artist(meta.clone()), ttl);
        meta
    }

    /// Search the catalog for artists. Plain query: the artist endpoint takes a bare name
    /// and the qualified form is dead everywhere now.
    pub async fn search_artists(&self, query: &str, limit: i32) -> Vec<ArtistHit> {
        if is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let key = dotnet::to_lower_invariant(&format!("ars|{query}|{limit}"));
        if let Some(Cached::ArtistHits(cached)) = self.get_cached(&key) {
            return cached;
        }
        // An artist page names its artist and lists its albums in two requests at once, and
        // both search for the name. They share one search. It runs without any one caller:
        // a caller giving up stops waiting, and the others still get their answer.
        let this = self.clone();
        let (query, fetch_key) = (query.to_string(), key.clone());
        self.inner
            .in_flight_artists
            .run(&key, async move {
                this.fetch_artist_search(&query, limit, fetch_key).await
            })
            .await
    }

    async fn fetch_artist_search(&self, query: &str, limit: i32, key: String) -> Vec<ArtistHit> {
        let mut hits = Vec::new();
        let attempt = async {
            let q = escape_data_string(query);
            let r = self
                .get_json(
                    &format!("{}/search/artist?q={q}&limit={limit}", self.base()),
                    false,
                )
                .await;
            // Caching an empty list on a refusal is what would make external artists
            // silently vanish from search3 for the rest of the process.
            if r.transient {
                return anyhow::Ok(Flow::Return(()));
            }
            if let Some(data) = data_array(r.doc.as_ref())? {
                for a in enumerate_array(data)? {
                    let id = match try_get_property(a, "id")? {
                        Some(aid @ Value::Number(_)) => Some(get_int64(aid)?.to_string()),
                        _ => None,
                    };
                    let name = str_prop(a, "name")?;
                    let (Some(id), Some(name)) = (id, name.filter(|n| !is_blank(n))) else {
                        continue;
                    };
                    hits.push(ArtistHit {
                        deezer_id: id,
                        name: name.to_string(),
                        picture_url: str_prop(a, "picture_xl")?
                            .or(str_prop(a, "picture_medium")?)
                            .map(str::to_string),
                        album_count: int(a, "nb_album")?.unwrap_or(0),
                        fans: int(a, "nb_fan")?.unwrap_or(0),
                    });
                }
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => return Vec::new(),
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer artist search '{query}' failed: {e}"),
        }

        let ttl = if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::ArtistHits(hits.clone()), ttl);
        hits
    }

    /// Search the album catalog. Single-track "albums" are dropped: a plain
    /// artist query returns a lot of them and they crowd out real records.
    pub async fn search_albums(&self, query: &str, limit: i32, keep_singles: bool) -> Vec<AlbumHit> {
        if is_blank(query) || limit <= 0 {
            return Vec::new();
        }
        let key = dotnet::to_lower_invariant(&format!(
            "as|{query}|{limit}|{}",
            if keep_singles { "True" } else { "False" }
        ));
        if let Some(Cached::AlbumHits(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut hits = Vec::new();
        let attempt = async {
            let q = escape_data_string(query);
            let r = self
                .get_json(
                    &format!("{}/search/album?q={q}&limit={limit}", self.base()),
                    false,
                )
                .await;
            // Caching an empty list here is what would make external albums silently
            // vanish from search3 for the rest of the process.
            if r.transient {
                return anyhow::Ok(Flow::Return(()));
            }
            if let Some(data) = data_array(r.doc.as_ref())? {
                for a in enumerate_array(data)? {
                    let id = match try_get_property(a, "id")? {
                        Some(aid @ Value::Number(_)) => Some(get_int64(aid)?.to_string()),
                        _ => None,
                    };
                    let title = str_prop(a, "title")?;
                    let (Some(id), Some(title)) = (id, title.filter(|t| !is_blank(t))) else {
                        continue;
                    };

                    let record_type = str_prop(a, "record_type")?;
                    let track_count = int(a, "nb_tracks")?.unwrap_or(0);
                    // Search lists albums; a one- or two-track single is a song there. The cover
                    // upgrade keeps them: a library of singles has their covers to replace.
                    if !keep_singles
                        && record_type.is_some_and(|t| dotnet::eq_ignore_case(t, "single"))
                        && track_count <= 2
                    {
                        continue;
                    }

                    let artist = match try_get_property(a, "artist")? {
                        Some(art) => str_prop(art, "name")?,
                        None => None,
                    };
                    hits.push(AlbumHit {
                        deezer_id: id,
                        title: title.to_string(),
                        artist: artist.unwrap_or("").to_string(),
                        cover_url: str_prop(a, "cover_xl")?
                            .or(str_prop(a, "cover_medium")?)
                            .map(str::to_string),
                        year: None,
                        track_count,
                        record_type: record_type.map(str::to_string),
                    });
                }
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => return Vec::new(),
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer album search '{query}' failed: {e}"),
        }

        let ttl = if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::AlbumHits(hits.clone()), ttl);
        hits
    }

    /// An artist's own releases for their page: albums, then EPs, then singles, then their own
    /// compilations, newest first within each, the way the apps group a discography. One copy
    /// of each title, whatever its type: the catalog lists a clean and an explicit copy of many,
    /// and an album can share its title with its own single or EP. The album is the one kept.
    /// Two releases of one title would also open as one, since an outside album's id is made
    /// from the artist and title, and the second would take over the first's. Singles used to
    /// be left out unless there was nothing else, which hid half of a career that is mostly
    /// singles; grouped after the records, they no longer bury them. The artist's name is not
    /// on this listing, so every hit carries the one given.
    pub async fn get_artist_albums(&self, deezer_artist_id: &str, artist_name: &str) -> Vec<AlbumHit> {
        if is_blank(deezer_artist_id) {
            return Vec::new();
        }
        let key = dotnet::to_lower_invariant(&format!("ara|{deezer_artist_id}"));
        if let Some(Cached::AlbumHits(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut releases: Vec<AlbumHit> = Vec::new();
        // Where each title sits in the list, so a better copy found later takes its place.
        let mut by_title: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut total: Option<i32> = None;
        let mut listed = 0usize;
        let attempt = async {
            let id = escape_data_string(deezer_artist_id);
            let r = self
                .get_json(
                    &format!("{}/artist/{id}/albums?limit={ARTIST_ALBUMS_LIMIT}", self.base()),
                    false,
                )
                .await;
            // Caching an empty list on a refusal would leave the artist's page empty for hours.
            if r.transient {
                return anyhow::Ok(Flow::Return(()));
            }
            if let Some(root) = &r.doc
                && let Some(data) = data_array(Some(root))?
            {
                total = int(root, "total")?;
                listed = array_length(data)?;
                for a in enumerate_array(data)? {
                    let album_id = match try_get_property(a, "id")? {
                        Some(aid @ Value::Number(_)) => Some(get_int64(aid)?.to_string()),
                        _ => None,
                    };
                    let title = str_prop(a, "title")?;
                    let (Some(album_id), Some(title)) = (album_id, title.filter(|t| !is_blank(t))) else {
                        continue;
                    };

                    let record_type = str_prop(a, "record_type")?.map(str::to_string);
                    let released = str_prop(a, "release_date")?;
                    let year = released.and_then(year_prefix).filter(|&y| y > 0);
                    let hit = AlbumHit {
                        deezer_id: album_id,
                        title: title.to_string(),
                        artist: artist_name.to_string(),
                        cover_url: str_prop(a, "cover_xl")?
                            .or(str_prop(a, "cover_medium")?)
                            .map(str::to_string),
                        year,
                        track_count: int(a, "nb_tracks")?.unwrap_or(0),
                        record_type,
                    };

                    let title_key = SongIdentity::key(title);
                    match by_title.get(&title_key) {
                        None => {
                            by_title.insert(title_key, releases.len());
                            releases.push(hit);
                        }
                        Some(&at) => {
                            if release_rank(hit.record_type.as_deref())
                                < release_rank(releases[at].record_type.as_deref())
                            {
                                releases[at] = hit;
                            }
                        }
                    }
                }
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(())) => return Vec::new(),
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer artist albums '{deezer_artist_id}' failed: {e}"),
        }

        // Said out loud, because a page that stops short otherwise looks like a whole career.
        if let Some(all) = total
            && all >= 0
            && all as usize > listed
        {
            warn!(
                "deezer artist {deezer_artist_id} ('{artist_name}') has {all} releases; the page lists the first {listed}"
            );
        }

        let mut hits = releases;
        hits.sort_by(|a, b| {
            release_rank(a.record_type.as_deref())
                .cmp(&release_rank(b.record_type.as_deref()))
                .then_with(|| b.year.unwrap_or(0).cmp(&a.year.unwrap_or(0)))
        });
        let ttl = if hits.is_empty() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(key, Cached::AlbumHits(hits.clone()), ttl);
        hits
    }

    /// A track count already known for an album, without asking the catalog. The outer
    /// `Option` is whether it is known; the inner one is the count, which may be known absent.
    pub fn try_known_track_count(&self, deezer_id: &str) -> Option<Option<i32>> {
        match self.get_cached(&format!("tc|{deezer_id}")) {
            Some(Cached::Number(count)) => Some(count),
            _ => None,
        }
    }

    /// How many tracks a catalog album has, from the album's own record, or None when it cannot
    /// be told. An artist's listing leaves the count out, and a page showing "0 songs" on every
    /// album reads as empty albums (some clients hide them). Asked in the interactive lane:
    /// someone is looking at the page, and the background lane is kept full by cache warming,
    /// which turned every one of these away. The caller keeps the number asked small.
    pub async fn album_track_count(&self, deezer_id: &str) -> Option<i32> {
        let key = format!("tc|{deezer_id}");
        if let Some(Cached::Number(cached)) = self.get_cached(&key) {
            return cached;
        }
        // Two visits to one page at once ask for the same albums; each album is asked once.
        let this = self.clone();
        let (id, fetch_key) = (deezer_id.to_string(), key.clone());
        self.inner
            .in_flight_counts
            .run(
                &key,
                async move { this.fetch_album_track_count(&id, fetch_key).await },
            )
            .await
    }

    async fn fetch_album_track_count(&self, deezer_id: &str, key: String) -> Option<i32> {
        let attempt = async {
            let r = self
                .get_json(
                    &format!("{}/album/{}", self.base(), escape_data_string(deezer_id)),
                    false,
                )
                .await;
            if r.transient {
                return anyhow::Ok(None);
            }
            let count = match &r.doc {
                Some(root) => int(root, "nb_tracks")?,
                None => None,
            };
            let ttl = if count.is_none() {
                NEGATIVE_TTL
            } else {
                POSITIVE_TTL
            };
            self.put(key, Cached::Number(count), ttl);
            Ok(count)
        };
        match attempt.await {
            Ok(count) => count,
            Err(e) => {
                debug!("deezer album {deezer_id} track count failed: {e}");
                None
            }
        }
    }

    /// Resolve an artist + album name to a Deezer album id. Needed because album
    /// ids minted from a song row carry no Deezer id, so the name is all we have.
    pub async fn find_album_id(&self, artist: &str, album: &str) -> Option<String> {
        if is_blank(album) {
            return None;
        }
        let key = dotnet::to_lower_invariant(&format!("ai|{artist}|{album}"));
        if let Some(Cached::Id(cached)) = self.get_cached(&key) {
            return cached;
        }

        let mut id = None;
        let attempt = async {
            let q = escape_data_string(&plain_query(&[artist, album]));
            let r = self
                .get_json(
                    &format!("{}/search/album?q={q}&limit={MATCH_CANDIDATES}", self.base()),
                    false,
                )
                .await;
            if r.transient {
                return anyhow::Ok(Flow::Return(None));
            }
            if let Some(a) = best_match(r.doc.as_ref(), artist, album)?
                && let Some(aid @ Value::Number(_)) = try_get_property(&a, "id")?
            {
                id = Some(get_int64(aid)?.to_string());
            }
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer album id lookup '{artist} - {album}' failed: {e}"),
        }

        let ttl = if id.is_none() { NEGATIVE_TTL } else { POSITIVE_TTL };
        self.put(key, Cached::Id(id.clone()), ttl);
        id
    }

    /// Album detail plus its full tracklist, ordered by disc then track position.
    /// One bounded request per resource; a release larger than the cap is reported as
    /// truncated rather than silently presented as complete.
    pub async fn get_album_detail(&self, deezer_id: &str) -> Option<AlbumDetail> {
        self.look_up_album_detail(deezer_id).await.detail
    }

    /// [`Self::get_album_detail`], saying also why there is no detail when there is none:
    /// Deezer has no such album, it has the album but no tracks for it, or it did not answer
    /// this time. Only the last may come right on its own a moment later.
    pub async fn look_up_album_detail(&self, deezer_id: &str) -> AlbumLookup {
        if is_blank(deezer_id) {
            return AlbumLookup {
                detail: None,
                answer: AlbumAnswer::NoSuchAlbum,
            };
        }
        let cache_key = format!("ad|{deezer_id}");
        if let Some(Cached::Album(cached)) = self.get_cached(&cache_key) {
            return cached;
        }
        let unavailable = AlbumLookup {
            detail: None,
            answer: AlbumAnswer::Unavailable,
        };

        let mut detail = None;
        // A tracklist we know is short gets a shorter life than a complete one, so a
        // truncated or partly-skipped album repairs itself instead of sticking.
        let mut partial = false;
        let attempt = async {
            let (mut title, mut artist, mut genre, mut label, mut cover) = (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            );
            let mut record_type = None;
            let mut year = None;
            // Kept from the album call: this is what tells an empty tracklist apart from an
            // album that genuinely has no tracks.
            let mut nb_tracks = None;

            {
                let r = self
                    .get_json(&format!("{}/album/{deezer_id}", self.base()), false)
                    .await;
                if r.transient {
                    return anyhow::Ok(Flow::Return(unavailable.clone()));
                }
                if let Some(root) = &r.doc {
                    nb_tracks = int(root, "nb_tracks")?;
                    title = str_prop(root, "title")?.unwrap_or("").to_string();
                    cover = str_prop(root, "cover_xl")?
                        .or(str_prop(root, "cover_medium")?)
                        .unwrap_or("")
                        .to_string();
                    label = str_prop(root, "label")?.unwrap_or("").to_string();
                    record_type = str_prop(root, "record_type")?.map(str::to_string);
                    if let Some(yr) = str_prop(root, "release_date")?.and_then(year_prefix) {
                        year = Some(yr);
                    }
                    if let Some(art) = try_get_property(root, "artist")? {
                        artist = str_prop(art, "name")?.unwrap_or("").to_string();
                    }
                    if let Some(name) = first_genre(root)? {
                        genre = name;
                    }
                }
            }

            // Deezer answered and there is no such album. Cacheable, but not forever.
            if is_blank(&title) {
                let none = AlbumLookup {
                    detail: None,
                    answer: AlbumAnswer::NoSuchAlbum,
                };
                self.put(cache_key.clone(), Cached::Album(none.clone()), NEGATIVE_TTL);
                return Ok(Flow::Return(none));
            }

            let mut tracks = Vec::new();
            {
                let tr = self
                    .get_json(
                        &format!("{}/album/{deezer_id}/tracks?limit=300", self.base()),
                        false,
                    )
                    .await;
                // The album call can succeed while the tracklist call is throttled. That
                // built a perfectly valid AlbumDetail carrying title, year and genre with
                // an empty tracklist, cached it permanently, and is why getAlbum reported
                // songCount 0 forever while still showing real metadata.
                if tr.transient {
                    return Ok(Flow::Return(unavailable.clone()));
                }
                if let Some(root) = &tr.doc
                    && let Some(data) = data_array(Some(root))?
                {
                    for t in enumerate_array(data)? {
                        let Some(t_title) = str_prop(t, "title")?.filter(|t| !is_blank(t)) else {
                            continue;
                        };
                        let t_artist = match try_get_property(t, "artist")? {
                            Some(ta) => str_prop(ta, "name")?,
                            None => None,
                        };
                        tracks.push(AlbumTrack {
                            title: t_title.to_string(),
                            artist: t_artist.map_or_else(|| artist.clone(), str::to_string),
                            duration: int(t, "duration")?,
                            track_position: int(t, "track_position")?,
                            disc_number: int(t, "disk_number")?,
                            isrc: str_prop(t, "isrc")?.map(str::to_string),
                        });
                    }

                    let total = int(root, "total")?;
                    if let Some(n) = total
                        && n >= 0
                        && n as usize > tracks.len()
                    {
                        partial = true;
                        warn!(
                            "deezer album '{title}' ({deezer_id}) returned {} of {n} tracks; tracklist is truncated",
                            tracks.len()
                        );
                    }
                }
            }

            // An empty tracklist on an album Deezer says HAS tracks is a failure, not an
            // answer. Absent nb_tracks counts as "has tracks": testing `nb_tracks > 0` alone
            // would let the empty result through and cache it exactly as before.
            if tracks.is_empty() && nb_tracks.is_none_or(|n| n > 0) {
                let expected =
                    nb_tracks.map_or_else(|| "an unknown number of".to_string(), |n| n.to_string());
                warn!(
                    "deezer album '{title}' ({deezer_id}) reports {expected} track(s) but returned none; not caching"
                );
                return Ok(Flow::Return(AlbumLookup {
                    detail: None,
                    answer: AlbumAnswer::NoTracks,
                }));
            }

            // Fewer tracks than the album claims, e.g. entries skipped for a blank title.
            if let Some(expected) = nb_tracks
                && (tracks.len() as i64) < i64::from(expected)
            {
                partial = true;
            }

            tracks.sort_by(|a, b| {
                a.disc_number
                    .unwrap_or(1)
                    .cmp(&b.disc_number.unwrap_or(1))
                    .then_with(|| {
                        a.track_position
                            .unwrap_or(i32::MAX)
                            .cmp(&b.track_position.unwrap_or(i32::MAX))
                    })
            });

            detail = Some(AlbumDetail {
                deezer_id: deezer_id.to_string(),
                title,
                artist,
                cover_url: (!cover.is_empty()).then_some(cover),
                year,
                genre: (!genre.is_empty()).then_some(genre),
                label: (!label.is_empty()).then_some(label),
                tracks,
                record_type,
            });
            Ok(Flow::Done)
        };
        match attempt.await {
            Ok(Flow::Return(answer)) => return answer,
            Ok(Flow::Done) => {}
            Err(e) => debug!("deezer album detail {deezer_id} failed: {e}"),
        }

        let ttl = match (&detail, partial) {
            (None, _) => NEGATIVE_TTL,
            (Some(_), true) => PARTIAL_TTL,
            (Some(_), false) => POSITIVE_TTL,
        };
        let lookup = match detail {
            None => unavailable,
            Some(detail) => AlbumLookup {
                detail: Some(detail),
                answer: AlbumAnswer::Found,
            },
        };
        self.put(cache_key, Cached::Album(lookup.clone()), ttl);
        lookup
    }

    /// The album's release year, and whether it could not be told this time. Shared across
    /// concurrent callers for the same album id (single-flight).
    async fn album_year(&self, album_id: i64) -> ElementResult<(Option<i32>, bool)> {
        if let Some(Cached::Number(year)) = self.get_cached(&format!("y|{album_id}")) {
            return Ok((year, false));
        }
        let this = self.clone();
        let answer = self
            .inner
            .album_year_tasks
            .run(&album_id.to_string(), async move {
                this.fetch_album_year(album_id).await.ok()
            })
            .await;
        // The fetch threw (an album record of the wrong shape): the awaiting caller saw the
        // exception in the C#.
        answer.ok_or(octo_core::json::element::ElementError::Format(
            "the album's release year",
        ))
    }

    async fn fetch_album_year(&self, album_id: i64) -> ElementResult<(Option<i32>, bool)> {
        let r = self
            .get_json(&format!("{}/album/{album_id}", self.base()), false)
            .await;

        // This used to be a raw indexer write after a bare catch, so it bypassed the
        // cache helper entirely and a throttled year lookup stuck permanently.
        if r.transient {
            return Ok((None, true));
        }

        let rd = match &r.doc {
            Some(root) => str_prop(root, "release_date")?,
            None => None,
        };
        let year = rd.and_then(year_prefix);
        let ttl = if year.is_none() {
            NEGATIVE_TTL
        } else {
            POSITIVE_TTL
        };
        self.put(format!("y|{album_id}"), Cached::Number(year), ttl);
        Ok((year, false))
    }

    /// An album's barcode (UPC), which names one exact release in every store, so the cover
    /// upgrade can find the same release at Apple in a batch instead of one search an album.
    pub async fn get_album_upc(&self, deezer_id: &str) -> Option<String> {
        if is_blank(deezer_id) {
            return None;
        }
        let key = format!("upc|{deezer_id}");
        if let Some(Cached::Text(cached)) = self.get_cached(&key) {
            return (!cached.is_empty()).then_some(cached);
        }
        let attempt = async {
            let r = self
                .get_json(
                    &format!("{}/album/{}", self.base(), escape_data_string(deezer_id)),
                    false,
                )
                .await;
            if r.transient {
                return anyhow::Ok(None);
            }
            let upc = match &r.doc {
                Some(root) => str_prop(root, "upc")?.map(str::to_string),
                None => None,
            };
            let upc = upc.filter(|u| !u.is_empty());
            let ttl = if upc.is_none() { NEGATIVE_TTL } else { POSITIVE_TTL };
            self.put(key.clone(), Cached::Text(upc.clone().unwrap_or_default()), ttl);
            Ok(upc)
        };
        match attempt.await {
            Ok(upc) => upc,
            Err(e) => {
                debug!("deezer album {deezer_id} barcode failed: {e}");
                None
            }
        }
    }

    async fn get_json(&self, url: &str, background: bool) -> DeezerResponse {
        // Genre names in album payloads localize to the caller's IP country
        // unless this header pins them. Read per request, so a settings
        // change reaches the next lookup without a restart.
        let language = accept_language_header::header_value(&self.inner.settings.current().metadata);
        let answer = match self.inner.http.get(url, background, language.as_deref()).await {
            Ok(answer) => answer,
            Err(e) => {
                debug!("deezer request {url} failed: {e}");
                return DeezerResponse::transient();
            }
        };
        // A 429 here is usually our OWN limiter shedding load rather than Deezer's,
        // and either way it is transient, so nothing derived from it is cached.
        if !answer.is_success() {
            return DeezerResponse::transient();
        }

        let doc: Value = match serde_json::from_str(&answer.text()) {
            Ok(doc) => doc,
            Err(e) => {
                debug!("deezer request {url} failed: {e}");
                return DeezerResponse::transient();
            }
        };

        // Deezer reports throttling as 200 + {"error":{"code":4,...}}, which parses
        // perfectly and then reads as "the album has no tracks". Catching it here is
        // what stops a quota blip becoming permanent cached state.
        if let Some(err @ Value::Object(_)) = doc.get("error") {
            let read = || -> ElementResult<(Option<i32>, Option<&str>, Option<&str>)> {
                Ok((
                    int(err, "code")?,
                    str_prop(err, "type")?,
                    str_prop(err, "message")?,
                ))
            };
            let (code, kind, message) = match read() {
                Ok(fields) => fields,
                Err(e) => {
                    debug!("deezer request {url} failed: {e}");
                    return DeezerResponse::transient();
                }
            };
            let definitive = code == Some(DEFINITIVE_ERROR_CODE);
            warn!(
                "deezer refused {url}: {} \"{}\" (code {}, treated as {})",
                kind.unwrap_or("(null)"),
                message.unwrap_or("(null)"),
                code.map_or_else(|| "(null)".to_string(), |c| c.to_string()),
                if definitive { "definitive" } else { "transient" }
            );
            return DeezerResponse {
                doc: None,
                transient: !definitive,
            };
        }

        DeezerResponse {
            doc: Some(doc),
            transient: false,
        }
    }

    /// The first track search hit that is this song, trying the song as asked and then the
    /// other ways `SongIdentity::query_variants` writes it ("suicideboys SUICIDE" for
    /// "$uicideboy$ $UICIDE", the title without its guests, the primary artist, the title
    /// alone). Every hit is judged against the song as asked, so a looser query never means a
    /// looser match. The first value says a search did not answer: nothing may be cached.
    async fn find_track(
        &self,
        artist: &str,
        title: &str,
        background: bool,
    ) -> ElementResult<(bool, Option<Value>)> {
        for variant in SongIdentity::query_variants(title, artist)
            .into_iter()
            .take(TRACK_SEARCHES)
        {
            let url = format!(
                "{}/search?q={}&limit={MATCH_CANDIDATES}",
                self.base(),
                escape_data_string(&variant.text())
            );
            let r = self.get_json(&url, background).await;
            if r.transient {
                return Ok((true, None));
            }
            if let Some(hit) = best_match(r.doc.as_ref(), artist, title)? {
                return Ok((false, Some(hit)));
            }
        }
        Ok((false, None))
    }

    /// Like [`Self::find_track`], but every hit that is this song from the first search that
    /// finds any, in the catalog's order.
    async fn find_track_hits(&self, artist: &str, title: &str) -> ElementResult<(bool, Vec<Value>)> {
        for variant in SongIdentity::query_variants(title, artist)
            .into_iter()
            .take(TRACK_SEARCHES)
        {
            let url = format!(
                "{}/search?q={}&limit={MATCH_CANDIDATES}",
                self.base(),
                escape_data_string(&variant.text())
            );
            let r = self.get_json(&url, false).await;
            if r.transient {
                return Ok((true, Vec::new()));
            }
            let hits = all_matches(r.doc.as_ref(), artist, title)?;
            if !hits.is_empty() {
                return Ok((false, hits));
            }
        }
        Ok((false, Vec::new()))
    }
}

/// The catalog call the release identifier makes. The service never fails; a throttled catalog
/// is `did_not_answer`.
#[async_trait::async_trait]
impl CatalogLookup for DeezerMetadataService {
    async fn enrich_track_candidates(
        &self,
        artist: &str,
        title: &str,
        max: i32,
    ) -> anyhow::Result<CatalogCandidates> {
        Ok((*DeezerMetadataService::enrich_track_candidates(self, artist, title, max).await).clone())
    }
}

/// Deezer no longer supports field-qualified search on the track endpoints. A query
/// like artist:"X" track:"Y" is now read as free text, so the literal words "artist"
/// and "track" have to appear in the record and nothing ever matches. Plain terms are
/// the only shape that still works.
fn plain_query(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !is_blank(p))
        .map(|p| p.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// What one field of a hit says about the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldVerdict {
    Match,
    Absent,
    Mismatch,
}

/// Compare the titles by `SongIdentity`: the same key, or the same key once stylized
/// characters are read as letters, or one key containing the other, because Deezer decorates
/// titles in ways no list names. Never another version: a live take or a remix carries its
/// own album and length, and attaching those to the original is wrong.
///
/// A field either side left empty is Absent, never a mismatch. Absent evidence is not
/// counter-evidence, and treating a field Deezer simply did not send as a contradiction would
/// throw away good hits the moment the payload shape changes.
fn compare_titles(want: &str, got: Option<&str>) -> FieldVerdict {
    let a = SongIdentity::parse_title(want, None);
    let b = SongIdentity::parse_title(got.unwrap_or(""), None);
    if a.key.is_empty() || b.key.is_empty() {
        return FieldVerdict::Absent;
    }
    if SongIdentity::distinct_versions(&a, None) != SongIdentity::distinct_versions(&b, None) {
        return FieldVerdict::Mismatch;
    }
    if a.key == b.key || a.loose_key == b.loose_key || a.key.contains(&b.key) || b.key.contains(&a.key) {
        FieldVerdict::Match
    } else {
        FieldVerdict::Mismatch
    }
}

/// The artists by `SongIdentity`, and one key containing the other, since Deezer credits
/// guests in the artist field.
fn compare_artists(want: &str, got: Option<&str>) -> FieldVerdict {
    let got = got.unwrap_or("");
    let a = SongIdentity::key(want);
    let b = SongIdentity::key(got);
    if a.is_empty() || b.is_empty() {
        return FieldVerdict::Absent;
    }
    if SongIdentity::artists_agree(want, got) || a.contains(&b) || b.contains(&a) {
        FieldVerdict::Match
    } else {
        FieldVerdict::Mismatch
    }
}

/// The first hit that positively matches on artist or title and contradicts on
/// neither. This is the guard that makes a plain query safe to use in place of the
/// qualified one: without it a near-miss at position 0 would be attached to the song
/// as fact. Requiring at least one positive match is what stops a hit that states
/// nothing at all from matching everything.
fn best_match(doc: Option<&Value>, artist: &str, title: &str) -> ElementResult<Option<Value>> {
    Ok(all_matches(doc, artist, title)?.into_iter().next())
}

/// Every hit that positively matches on artist or title and contradicts on neither,
/// in the catalog's order. The first is what [`best_match`] returns.
fn all_matches(doc: Option<&Value>, artist: &str, title: &str) -> ElementResult<Vec<Value>> {
    let mut hits = Vec::new();
    let Some(data) = data_array(doc)? else {
        return Ok(hits);
    };
    for hit in enumerate_array(data)? {
        let title_verdict = compare_titles(title, str_prop(hit, "title")?);
        let hit_artist = match try_get_property(hit, "artist")? {
            Some(a) => str_prop(a, "name")?,
            None => None,
        };
        let artist_verdict = compare_artists(artist, hit_artist);

        if title_verdict == FieldVerdict::Mismatch || artist_verdict == FieldVerdict::Mismatch {
            continue;
        }
        if title_verdict == FieldVerdict::Match || artist_verdict == FieldVerdict::Match {
            hits.push(hit.clone());
        }
    }
    Ok(hits)
}

/// `root.data` when it is an array. Reading a property of a root that is not an object
/// throws, as `TryGetProperty` did.
fn data_array(doc: Option<&Value>) -> ElementResult<Option<&Value>> {
    let Some(root) = doc else {
        return Ok(None);
    };
    Ok(match try_get_property(root, "data")? {
        Some(data @ Value::Array(_)) => Some(data),
        _ => None,
    })
}

fn first_data(doc: Option<&Value>) -> ElementResult<Option<&Value>> {
    Ok(match data_array(doc)? {
        Some(Value::Array(items)) => items.first(),
        _ => None,
    })
}

/// `genres.data[0].name`, when the album lists a genre.
fn first_genre(root: &Value) -> ElementResult<Option<String>> {
    if let Some(genres) = try_get_property(root, "genres")?
        && let Some(gd @ Value::Array(items)) = try_get_property(genres, "data")?
        && array_length(gd)? > 0
    {
        return Ok(str_prop(&items[0], "name")?.map(str::to_string));
    }
    Ok(None)
}

/// The C# `Int` helper: `GetInt32()` of a number, which throws for one that is not an
/// integer in range.
fn int(element: &Value, name: &str) -> ElementResult<Option<i32>> {
    match try_get_property(element, name)? {
        Some(value @ Value::Number(_)) => Ok(Some(get_int32(value)?)),
        _ => Ok(None),
    }
}

/// `rd.Length >= 4 && int.TryParse(rd[..4], out var yr)`: the first four UTF-16 units read as an
/// integer, with the leading and trailing white space and the sign `int.TryParse` allows.
fn year_prefix(release_date: &str) -> Option<i32> {
    if dotnet::utf16_len(release_date) < 4 {
        return None;
    }
    let mut prefix = String::new();
    let mut units = 0;
    for c in release_date.chars() {
        units += c.len_utf16();
        if units > 4 {
            // A surrogate pair cut in half is not a digit either way.
            return None;
        }
        prefix.push(c);
        if units == 4 {
            break;
        }
    }
    let white = |c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ');
    let text = prefix.trim_matches(white);
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
#[path = "deezer_metadata_service_tests.rs"]
mod tests;
