//! The slice of `Microsoft.Extensions.Caching.Memory.MemoryCache` the Subsonic services used:
//! entries of size 1 under a `SizeLimit`, each with an absolute lifetime and optionally a
//! sliding one renewed on every read.
//!
//! As with `MemoryCache`, an entry that would take the cache past its limit is not added, and
//! the attempt starts a compaction that drops expired entries and then the least recently used
//! 5%, so later entries fit again.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

struct Slot<V> {
    value: V,
    /// When the entry dies regardless of reads (`AbsoluteExpirationRelativeToNow`).
    absolute: Instant,
    /// `SlidingExpiration`, and when it runs out next.
    sliding: Option<(Duration, Instant)>,
    last_access: Instant,
}

impl<V> Slot<V> {
    fn expired(&self, now: Instant) -> bool {
        now >= self.absolute || self.sliding.is_some_and(|(_, until)| now >= until)
    }
}

pub(crate) struct ExpiringCache<V> {
    entries: Mutex<HashMap<String, Slot<V>>>,
    capacity: usize,
}

/// `MemoryCacheOptions.CompactionPercentage`'s default.
const COMPACTION_PERCENTAGE: f64 = 0.05;

impl<V: Clone> ExpiringCache<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        ExpiringCache {
            entries: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// `TryGetValue`: the live value, renewing its sliding lifetime.
    pub(crate) fn get(&self, key: &str) -> Option<V> {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        let slot = entries.get_mut(key)?;
        if slot.expired(now) {
            entries.remove(key);
            return None;
        }
        slot.last_access = now;
        if let Some((window, until)) = &mut slot.sliding {
            *until = now + *window;
        }
        Some(slot.value.clone())
    }

    /// `Set` with an absolute lifetime and, optionally, a sliding one.
    pub(crate) fn set(&self, key: &str, value: V, absolute: Duration, sliding: Option<Duration>) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        let replacing = entries.contains_key(key);
        if !replacing && entries.len() >= self.capacity {
            // Over capacity: this entry is not added, and the cache compacts for the next one.
            entries.retain(|_, slot| !slot.expired(now));
            if entries.len() >= self.capacity {
                let drop = ((entries.len() as f64) * COMPACTION_PERCENTAGE).ceil() as usize;
                let mut by_age: Vec<(String, Instant)> =
                    entries.iter().map(|(k, s)| (k.clone(), s.last_access)).collect();
                by_age.sort_by_key(|(_, at)| *at);
                for (k, _) in by_age.into_iter().take(drop.max(1)) {
                    entries.remove(&k);
                }
                return;
            }
        }
        entries.insert(
            key.to_string(),
            Slot {
                value,
                absolute: now + absolute,
                sliding: sliding.map(|window| (window, now + window)),
                last_access: now,
            },
        );
    }

    /// `Count`: entries held, expired ones not yet swept included, as `MemoryCache.Count` was.
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_expire_and_slide() {
        let cache = ExpiringCache::new(4);
        cache.set("a", 1, Duration::from_millis(40), None);
        cache.set("b", 2, Duration::from_secs(60), Some(Duration::from_millis(40)));
        assert_eq!(cache.get("a"), Some(1));
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(cache.get("b"), Some(2));
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(cache.get("a"), None);
        // Read 25 ms ago, so its 40 ms window was renewed.
        assert_eq!(cache.get("b"), Some(2));
    }

    #[test]
    fn a_full_cache_drops_the_new_entry_and_compacts() {
        let cache = ExpiringCache::new(2);
        cache.set("a", 1, Duration::from_secs(60), None);
        cache.set("b", 2, Duration::from_secs(60), None);
        cache.set("c", 3, Duration::from_secs(60), None);
        assert_eq!(cache.get("c"), None);
        assert_eq!(cache.len(), 1);
        cache.set("c", 3, Duration::from_secs(60), None);
        assert_eq!(cache.get("c"), Some(3));
    }
}
