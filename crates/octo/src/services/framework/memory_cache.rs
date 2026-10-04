//! `Microsoft.Extensions.Caching.Memory.MemoryCache` with a `SizeLimit`, as the metadata clients
//! and the cover-art aggregator used it: every entry has a size and an absolute expiry relative
//! to when it was set. Not a C# file of Octo's; the framework type those were built on.
//!
//! An entry that would take the cache over its limit is not stored, and the cache is compacted
//! down to 95% of the limit (`CompactionPercentage` 0.05): expired entries first, then the ones
//! read least recently. .NET compacts on a background thread; this does it in place.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

struct Entry<V> {
    value: V,
    size: u64,
    expires: Instant,
    last_access: Instant,
}

struct State<V> {
    entries: HashMap<String, Entry<V>>,
    size: u64,
}

pub struct MemoryCache<V> {
    size_limit: u64,
    state: Mutex<State<V>>,
}

impl<V: Clone> MemoryCache<V> {
    pub fn new(size_limit: u64) -> Self {
        Self {
            size_limit,
            state: Mutex::new(State {
                entries: HashMap::new(),
                size: 0,
            }),
        }
    }

    /// `TryGetValue`: the value while it has not expired.
    pub fn get(&self, key: &str) -> Option<V> {
        let now = Instant::now();
        let mut state = self.state.lock();
        let expired = match state.entries.get_mut(key) {
            None => return None,
            Some(entry) if entry.expires > now => {
                entry.last_access = now;
                return Some(entry.value.clone());
            }
            Some(_) => true,
        };
        if expired {
            Self::remove(&mut state, key);
        }
        None
    }

    /// `Set` with `Size` and `AbsoluteExpirationRelativeToNow`.
    pub fn set(&self, key: impl Into<String>, value: V, size: u64, ttl: Duration) {
        let key = key.into();
        let now = Instant::now();
        let mut state = self.state.lock();
        // The prior entry goes whether or not the new one fits.
        Self::remove(&mut state, &key);
        if state.size.saturating_add(size) > self.size_limit {
            self.compact(&mut state, now);
            return;
        }
        state.size += size;
        state.entries.insert(
            key,
            Entry {
                value,
                size,
                expires: now + ttl,
                last_access: now,
            },
        );
    }

    /// `Clear()`.
    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.entries.clear();
        state.size = 0;
    }

    pub fn len(&self) -> usize {
        self.state.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn remove(state: &mut State<V>, key: &str) {
        if let Some(entry) = state.entries.remove(key) {
            state.size -= entry.size;
        }
    }

    fn compact(&self, state: &mut State<V>, now: Instant) {
        let expired: Vec<String> = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.expires <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            Self::remove(state, &key);
        }

        let low_watermark = (self.size_limit as f64 * 0.95) as u64;
        if state.size <= low_watermark {
            return;
        }
        let mut by_age: Vec<(Instant, String)> = state
            .entries
            .iter()
            .map(|(key, entry)| (entry.last_access, key.clone()))
            .collect();
        by_age.sort();
        for (_, key) in by_age {
            if state.size <= low_watermark {
                break;
            }
            Self::remove(state, &key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn entries_expire_and_an_entry_over_the_limit_is_not_kept() {
        let cache = MemoryCache::new(10);
        cache.set("a", 1, 4, Duration::from_secs(60));
        assert_eq!(cache.get("a"), Some(1));
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(cache.get("a"), None);

        cache.set("b", 2, 6, Duration::from_secs(60));
        tokio::time::advance(Duration::from_secs(1)).await;
        cache.set("c", 3, 4, Duration::from_secs(60));
        tokio::time::advance(Duration::from_secs(1)).await;
        cache.get("b");
        // 6 + 4 + 1 > 10: "d" is turned away, and the cache is brought under 9.5 by dropping
        // the least recently read ("c").
        cache.set("d", 4, 1, Duration::from_secs(60));
        assert_eq!(cache.get("d"), None);
        assert_eq!(cache.get("c"), None);
        assert_eq!(cache.get("b"), Some(2));
    }
}
