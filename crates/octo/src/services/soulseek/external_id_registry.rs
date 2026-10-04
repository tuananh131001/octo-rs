//! Port of `Services/Soulseek/ExternalIdRegistry.cs`, the store behind `external-ids.json`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use octo_core::common::SongIdentity;
use octo_core::soulseek::{LengthSource, RoutingKind, SongLength, SoulseekRouting};
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::services::state_file;

/// A routing the registry and its callers share. The C# registry stored the very object a
/// caller registered and handed the same object back from `Lookup`, and callers changed it in
/// place (pinning a video, settling a catalog artist), so here it is one routing behind a
/// mutex. The mutex also stands in for the `lock (routing)` of `SongLength`.
///
/// Lock order: the registry never takes a routing's lock while holding its own, so a caller
/// may hold a routing locked and still call into the registry.
#[derive(Debug, Clone, Default)]
pub struct SharedRouting(Arc<Mutex<SoulseekRouting>>);

impl SharedRouting {
    pub fn new(routing: SoulseekRouting) -> Self {
        SharedRouting(Arc::new(Mutex::new(routing)))
    }

    /// The routing, locked for reading or changing in place.
    pub fn lock(&self) -> MutexGuard<'_, SoulseekRouting> {
        self.0.lock()
    }

    /// A copy of the routing as it is now.
    pub fn snapshot(&self) -> SoulseekRouting {
        self.0.lock().clone()
    }

    /// Whether both are the same routing (C# `ReferenceEquals`).
    pub fn ptr_eq(&self, other: &SharedRouting) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// `SongLength.Remember` under the routing's lock.
    pub fn remember_length(&self, seconds: Option<i32>, source: LengthSource) -> bool {
        SongLength::remember(&mut self.0.lock(), seconds, source)
    }

    /// `SongLength.Shown` under the routing's lock.
    pub fn shown_length(&self) -> (Option<i32>, LengthSource) {
        SongLength::shown(&self.0.lock())
    }
}

impl From<SoulseekRouting> for SharedRouting {
    fn from(routing: SoulseekRouting) -> Self {
        SharedRouting::new(routing)
    }
}

/// Server-side registry that maps short opaque IDs (Navidrome-shaped 22-char base62)
/// to Soulseek/YouTube routing info. Subsonic clients are picky about song-id format —
/// some quietly drop entries with long pipe-delimited IDs from their play queues.
/// Translating to a short, alphabetic-looking key avoids that whole class of issue.
///
/// IDs are deterministic (sha256-derived) so the same routing always produces the
/// same id; this keeps caches/de-duplication on the client side stable across calls.
/// The dictionary is LRU-bounded so a single user can't grow it without limit.
///
/// ## Persistence
///
/// The registry is the ONLY thing that knows an id is ours: ParseExternalId decides
/// "external" by looking it up here. So when this was memory-only, every restart made
/// every id a client still held look local, and those ids were relayed to Navidrome,
/// which has no such media and answers error 70 "data not found". Clients surface that
/// per play and per poll, which reads as a stream of errors from a working server.
///
/// Ids are deterministic, so re-running the search that minted one brings it back. That
/// is a recovery, not a design: a queue built before a restart has no reason to search
/// again. Writing the map down is what makes an id outlive the process.
///
/// Flushing: [`ExternalIdRegistry::flush`] writes when something changed. The C# ran it from a
/// 10 s timer and from `Dispose`; here [`ExternalIdRegistry::run_flusher`] is the timer (a
/// worker, which flushes once more on shutdown) and `Drop` is the `Dispose`.
pub struct ExternalIdRegistry {
    path: Option<PathBuf>,
    lru: Mutex<Lru>,
    dirty: AtomicBool,
    flush_lock: Mutex<()>,
}

/// The entries and their recency. The C# kept a `ConcurrentDictionary` plus a `LinkedList`
/// moved to front on every touch; a stamp per entry and a map from stamp to id is the same
/// order without the linear `LinkedList.Remove`.
#[derive(Default)]
struct Lru {
    by_id: HashMap<String, (SharedRouting, u64)>,
    /// Stamp → id; the highest stamp is the most recently used.
    order: BTreeMap<u64, String>,
    next: u64,
}

impl Lru {
    fn touch(&mut self, id: &str) {
        let stamp = self.next;
        self.next += 1;
        if let Some((_, old)) = self.by_id.get_mut(id) {
            self.order.remove(old);
            *old = stamp;
            self.order.insert(stamp, id.to_string());
        }
    }

    fn trim(&mut self) {
        while self.by_id.len() > ExternalIdRegistry::MAX_ENTRIES {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.by_id.remove(&oldest);
        }
    }

    /// Most recently used first.
    fn ids(&self) -> Vec<String> {
        self.order.values().rev().cloned().collect()
    }
}

/// One line of `external-ids.json` (the private record `Persisted(string Id, SoulseekRouting Routing)`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Persisted {
    #[serde(deserialize_with = "state_file::null_as_default")]
    id: String,
    routing: Option<SoulseekRouting>,
}

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

impl ExternalIdRegistry {
    pub const MAX_ENTRIES: usize = 10_000;

    /// A search registers well over a hundred routings, so flushing per write
    /// would turn one search into a hundred file writes. Coalesce instead: the window is
    /// short next to the restarts this exists to survive.
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

    /// In memory only (`new ExternalIdRegistry()`), for tests.
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    /// A registry kept in the file at `path` (loaded now), or in memory only when the path is
    /// absent or blank.
    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = path
            .map(|p| p.as_ref().to_path_buf())
            .filter(|p| !p.as_os_str().to_string_lossy().trim().is_empty());
        let registry = ExternalIdRegistry {
            path,
            lru: Mutex::new(Lru::default()),
            dirty: AtomicBool::new(false),
            flush_lock: Mutex::new(()),
        };
        if registry.path.is_some() {
            registry.load();
        }
        registry
    }

    pub fn register(&self, routing: impl Into<SharedRouting>) -> String {
        let routing = routing.into();
        let id = Self::make_short_id(&routing.lock());
        let previous = self.lru.lock().by_id.get(&id).map(|(r, _)| r.clone());

        if let Some(previous) = previous.filter(|p| !p.ptr_eq(&routing)) {
            // Read the earlier routing under its own lock, then let it go before taking the
            // new one's, so two registrations never hold both.
            let (prev_album_id, (seconds, source), prev_isrc, prev_artist_id) = {
                let p = previous.lock();
                (
                    p.external_album_id.clone(),
                    SongLength::shown(&p),
                    p.isrc.clone(),
                    p.external_artist_id.clone(),
                )
            };
            let mut r = routing.lock();
            // A song row mints an album routing with no Deezer id (ConvertSongToJson), and it
            // hashes to the same id as the one an album search already resolved. Registering it
            // must not erase that id. Best-effort only: this is an optimization, and losing the
            // race just sends GetAlbumAsync down its cached name-lookup fallback.
            if r.external_album_id.is_none() && prev_album_id.is_some() {
                r.external_album_id = prev_album_id;
            }
            // Every search and every station playlist mints its songs again, and each mint is a
            // fresh routing. Without this, the length a lookup found for a song lasted until the
            // next search for it, which is why a row that had a length could lose it again.
            if source > r.shown_duration_source {
                r.shown_duration = seconds;
                r.shown_duration_source = source;
            }
            // The same for the ISRC an album listing found: a search row for the same song
            // names none, and must not forget it.
            if r.isrc.is_none() {
                r.isrc = prev_isrc;
            }
            // And the catalog artist an artist search or page settled on: every album row
            // mints its artist again by name alone, and must not undo that choice.
            if r.external_artist_id.is_none() {
                r.external_artist_id = prev_artist_id;
            }
        }

        let mut lru = self.lru.lock();
        let stamp = lru.next;
        lru.next += 1;
        if let Some((_, old)) = lru.by_id.insert(id.clone(), (routing, stamp)) {
            lru.order.remove(&old);
        }
        lru.order.insert(stamp, id.clone());
        lru.trim();
        drop(lru);
        self.dirty.store(true, Ordering::SeqCst);
        id
    }

    pub fn lookup(&self, short_id: &str) -> Option<SharedRouting> {
        let mut lru = self.lru.lock();
        let routing = lru.by_id.get(short_id).map(|(r, _)| r.clone())?;
        lru.touch(short_id);
        Some(routing)
    }

    /// How many ids the registry holds.
    pub fn count(&self) -> usize {
        self.lru.lock().by_id.len()
    }

    /// The songs a song row filed under the album `album` by `artist`, newest first, one per
    /// recording. A row names its album after the song's album, or after its own title when it
    /// has none (SubsonicResponseBuilder.ConvertSongFields), so this matches on the same rule,
    /// plus the title alone, for a song whose album was filled in after its row named one.
    /// For getAlbum when the catalog cannot list the album (#59). `limit` was 50 by default.
    pub fn songs_filed_under(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
        limit: usize,
    ) -> Vec<(String, SharedRouting)> {
        let (Some(artist), Some(album)) = (artist.filter(|a| !a.is_empty()), album.filter(|a| !a.is_empty()))
        else {
            return Vec::new();
        };
        let order: Vec<(String, SharedRouting)> = {
            let lru = self.lru.lock();
            lru.ids()
                .into_iter()
                .filter_map(|id| lru.by_id.get(&id).map(|(r, _)| (id, r.clone())))
                .collect()
        };

        let mut found = Vec::new();
        let mut recordings = HashSet::new();
        for (id, shared) in order {
            let recording = {
                let routing = shared.lock();
                if routing.kind != RoutingKind::Song {
                    continue;
                }
                if routing.artist.as_deref() != Some(artist) {
                    continue;
                }
                let filed_under = if routing.album.as_deref().is_none_or(|a| a.trim().is_empty()) {
                    routing.title.as_deref()
                } else {
                    routing.album.as_deref()
                };
                if filed_under != Some(album) && routing.title.as_deref() != Some(album) {
                    continue;
                }
                SongIdentity::key(routing.title.as_deref().unwrap_or(""))
            };
            let recording = if recording.is_empty() {
                id.clone()
            } else {
                recording
            };
            if !recordings.insert(recording) {
                continue;
            }
            found.push((id, shared));
            if found.len() >= limit {
                break;
            }
        }
        found
    }

    /// Store a length for a song under `short_id`, by the rules in [`SongLength`]. Looked up by
    /// id at write time rather than handed a routing, because a background lookup can outlive
    /// the routing it started from: a search that ran meanwhile replaced it, and a write to the
    /// old object would be lost.
    pub fn remember_length(&self, short_id: &str, seconds: Option<i32>, source: LengthSource) -> bool {
        if short_id.is_empty() {
            return false;
        }
        let Some(routing) = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone()) else {
            return false;
        };
        {
            let mut r = routing.lock();
            if r.kind != RoutingKind::Song {
                return false;
            }
            if !SongLength::remember(&mut r, seconds, source) {
                return false;
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
        true
    }

    /// Derive 22 base62 chars from sha256 of routing fields. Same input -> same id.
    /// The Kind prefix is critical: a song "Drake - Hotline Bling" must hash to a
    /// different id than the album "Hotline Bling" or the artist "Drake", or
    /// getCoverArt would return the wrong scope's artwork.
    pub fn make_short_id(r: &SoulseekRouting) -> String {
        let text = |s: &Option<String>| s.clone().unwrap_or_default();
        let seed = match r.kind {
            RoutingKind::Album => format!("k:album|a:{}|al:{}", text(&r.artist), text(&r.album)),
            RoutingKind::Artist => format!("k:artist|a:{}", text(&r.artist)),
            RoutingKind::Song => format!(
                "k:song|yt:{}|a:{}|t:{}|d:{}",
                text(&r.you_tube_id),
                text(&r.artist),
                text(&r.title),
                r.duration.map(|d| d.to_string()).unwrap_or_default()
            ),
        };
        let hash = Sha256::digest(seed.as_bytes());
        Self::to_base62(&hash, 22)
    }

    /// Treat the first 16 bytes as a big integer and base62-encode it, least significant digit
    /// first. We don't need strict cryptographic uniqueness — just collision resistance within
    /// ~10k items.
    fn to_base62(bytes: &[u8], length: usize) -> String {
        let mut head = [0u8; 16];
        head.copy_from_slice(&bytes[..16]);
        let mut value = u128::from_be_bytes(head);
        let mut out = String::with_capacity(length);
        while out.len() < length {
            let rem = (value % 62) as usize;
            value /= 62;
            out.push(ALPHABET[rem] as char);
            if value == 0 {
                break;
            }
        }
        while out.len() < length {
            out.push('0');
        }
        out.truncate(length);
        out
    }

    fn load(&self) {
        let Some(path) = &self.path else { return };
        let result = (|| -> anyhow::Result<()> {
            let Some(text) = state_file::read_text(path)? else {
                return Ok(());
            };
            let entries: Option<Vec<Persisted>> = serde_json::from_str(&text)?;
            let Some(entries) = entries else { return Ok(()) };

            // Stored most-recently-used first, so replaying in order rebuilds the same
            // eviction order rather than an arbitrary one.
            let mut lru = self.lru.lock();
            let mut stamp = entries.len() as u64;
            for e in entries {
                let Some(routing) = e.routing else { continue };
                if e.id.is_empty() {
                    continue;
                }
                match lru.by_id.get_mut(&e.id) {
                    // A duplicate id: the later routing wins, as `_byId[e.Id] = ...` did, at
                    // the earlier (more recent) place.
                    Some((r, _)) => *r = SharedRouting::new(routing),
                    None => {
                        lru.by_id
                            .insert(e.id.clone(), (SharedRouting::new(routing), stamp));
                        lru.order.insert(stamp, e.id);
                        stamp -= 1;
                    }
                }
            }
            lru.next = lru.order.keys().next_back().map_or(0, |s| s + 1);
            lru.trim();
            let count = lru.by_id.len();
            drop(lru);
            info!("external id registry restored {count} entries");
            Ok(())
        })();
        if let Err(e) = result {
            // A registry that will not load is a cold start, not a failure to boot.
            warn!("external id registry could not be read: {e}");
        }
    }

    /// Writes the registry when anything changed since the last write.
    pub fn flush(&self) {
        let Some(path) = &self.path else { return };
        let _flushing = self.flush_lock.lock();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let order: Vec<(String, SharedRouting)> = {
            let lru = self.lru.lock();
            lru.ids()
                .into_iter()
                .filter_map(|id| lru.by_id.get(&id).map(|(r, _)| (id, r.clone())))
                .collect()
        };
        let entries: Vec<Persisted> = order
            .into_iter()
            .map(|(id, routing)| Persisted {
                id,
                routing: Some(routing.snapshot()),
            })
            .collect();
        if let Err(e) = state_file::save_atomic(path, &octo_core::json::to_string(&entries)) {
            // Best-effort: losing a flush costs the ids minted since the last one, which
            // is the behaviour we already had. It must never take a request down.
            self.dirty.store(true, Ordering::SeqCst);
            warn!("external id registry could not be written: {e}");
        }
    }

    /// The flush timer, as a worker: every [`Self::FLUSH_INTERVAL`], and once more on shutdown.
    pub async fn run_flusher(
        self: Arc<Self>,
        token: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let registry = self.clone();
        state_file::flush_every(Self::FLUSH_INTERVAL, token, Arc::new(move || registry.flush())).await
    }
}

impl Drop for ExternalIdRegistry {
    /// `Dispose`: the last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
#[path = "external_id_registry_tests.rs"]
mod tests;
