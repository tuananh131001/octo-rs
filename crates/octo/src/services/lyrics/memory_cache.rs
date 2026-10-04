//! The part of `Microsoft.Extensions.Caching.Memory.MemoryCache` the lyrics services used: a
//! size limit with every entry of size 1, and an absolute expiry per entry. Not a C# file.
//!
//! As in .NET, an entry that would take the cache over its limit is not added, and the cache
//! is compacted instead: expired entries first, then the least recently used, down to 95% of
//! the limit. Times are tokio's, so a test with a paused clock can move them.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

struct Entry<V> {
    value: V,
    expires: Instant,
    last_access: Instant,
}

pub(crate) struct MemoryCache<V> {
    size_limit: usize,
    entries: Mutex<HashMap<String, Entry<V>>>,
}

impl<V: Clone> MemoryCache<V> {
    pub(crate) fn new(size_limit: usize) -> Self {
        Self {
            size_limit,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// `TryGetValue`: the value while it has not expired.
    pub(crate) fn get(&self, key: &str) -> Option<V> {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(key)?;
        if entry.expires <= now {
            entries.remove(key);
            return None;
        }
        entry.last_access = now;
        Some(entry.value.clone())
    }

    /// `Set(key, value, AbsoluteExpirationRelativeToNow = ttl)`.
    pub(crate) fn set(&self, key: &str, value: V, ttl: Duration) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        if !entries.contains_key(key) && entries.len() >= self.size_limit {
            entries.retain(|_, entry| entry.expires > now);
            if entries.len() >= self.size_limit {
                // Over capacity: this entry is dropped and the least recently used go.
                let keep = self.size_limit * 95 / 100;
                let mut by_age: Vec<(String, Instant)> = entries
                    .iter()
                    .map(|(key, entry)| (key.clone(), entry.last_access))
                    .collect();
                by_age.sort_by_key(|(_, at)| *at);
                let excess = entries.len().saturating_sub(keep);
                for (old, _) in by_age.into_iter().take(excess) {
                    entries.remove(&old);
                }
                return;
            }
        }
        entries.insert(
            key.to_string(),
            Entry {
                value,
                expires: now + ttl,
                last_access: now,
            },
        );
    }

    /// `Clear()`.
    pub(crate) fn clear(&self) {
        self.entries.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_entry_expires_and_a_full_cache_drops_the_newcomer_and_compacts() {
        let cache = MemoryCache::new(20);
        cache.set("a", 1, Duration::from_secs(10));
        assert_eq!(cache.get("a"), Some(1));
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(cache.get("a"), None);

        for n in 0..20 {
            cache.set(&n.to_string(), n, Duration::from_secs(60));
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        cache.set("new", 99, Duration::from_secs(60));
        assert_eq!(cache.get("new"), None);
        // Down to 95%: the oldest one went.
        assert_eq!(cache.get("0"), None);
        assert_eq!(cache.get("1"), Some(1));
        cache.set("new", 99, Duration::from_secs(60));
        assert_eq!(cache.get("new"), Some(99));

        cache.clear();
        assert_eq!(cache.get("1"), None);
    }
}
