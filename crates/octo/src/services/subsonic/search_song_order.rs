//! Port of `Services/Subsonic/SearchSongOrder.cs`: what page one of a search showed, and the
//! cache that keeps it for the pages after.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use octo_core::models::domain::Song;
use octo_subsonic::subsonic_model_mapper::{self, SearchRow};
use parking_lot::Mutex;

use super::expiring_cache::ExpiringCache;

/// What page one of a search showed, kept so the pages after it can carry on from it. See
/// `SearchSongPagePlanner` for how a later page is placed.
#[derive(Debug, Clone)]
pub struct SearchSongOrder {
    /// The outside songs page one sliced from. The same frozen list `ExternalSearchService`
    /// handed out, never a copy and never mutated.
    pub built: Arc<Vec<Song>>,
    /// The songCount page one was asked for.
    pub page_one_count: i32,
    /// How many library rows page one asked Navidrome for.
    pub prefix_size: i32,
    /// How many it got.
    pub prefix_count: i32,
    /// How many rows of `built` page one took.
    pub page_one_externals: i32,
    /// The rest of `built`, less any song page one already listed from the library. Removed
    /// here rather than on each page so a later page has no gaps in it.
    pub later_externals: Vec<Song>,
    /// Dedup keys of the library rows page one showed.
    pub prefix_keys: HashSet<String>,
}

impl SearchSongOrder {
    /// The library only has more to give when page one got every row it asked for.
    pub fn library_continues(&self) -> bool {
        self.prefix_count >= self.prefix_size && self.prefix_size > 0
    }

    /// How many outside rows page one shows: its share of the budget, plus whatever the
    /// library left unused, never more than were built. The search path and a rebuilt order
    /// both use this, so a rebuild places page one exactly where it fell.
    pub fn page_one_external_count(
        built_count: i32,
        local_target: i32,
        external_target: i32,
        local_returned: i32,
    ) -> i32 {
        built_count.min(external_target + (local_target - local_returned).max(0))
    }

    /// The order for a page one built from `built` and answered with `local_songs` (rows as
    /// `SubsonicModelMapper.ParseSearchResponse` returns them).
    pub fn from(
        built: Arc<Vec<Song>>,
        requested_songs: i32,
        local_target: i32,
        external_target: i32,
        local_songs: &[SearchRow],
    ) -> SearchSongOrder {
        let shown = Self::page_one_external_count(
            built.len() as i32,
            local_target,
            external_target,
            local_songs.len() as i32,
        );
        let owned = subsonic_model_mapper::local_song_keys(local_songs);
        let later = built
            .iter()
            .skip(shown.max(0) as usize)
            .filter(|song| !subsonic_model_mapper::is_listed(song, &owned))
            .cloned()
            .collect();
        SearchSongOrder {
            built,
            page_one_count: requested_songs.max(0),
            prefix_size: local_target,
            prefix_count: local_songs.len() as i32,
            page_one_externals: shown,
            later_externals: later,
            prefix_keys: owned,
        }
    }
}

/// How long an entry is kept after it was last read. Long enough to scroll a result list at
/// reading pace; short enough that a search run again later is built fresh. Reading an entry
/// renews it, so a list still being scrolled is kept.
pub(crate) const IDLE: Duration = Duration::from_secs(10 * 60);

/// A hard ceiling so an order still being scrolled is also rebuilt eventually.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Entries kept at once. An order holds at most a build's worth of songs (60) that are shared
/// with the build, and an entry at most [`ORDERS_PER_ENTRY`] of them, so this is a few hundred
/// small lists at worst.
pub(crate) const CAPACITY: usize = 256;

/// Page-one sizes one entry keeps. A client uses one or two; the oldest goes first.
pub(crate) const ORDERS_PER_ENTRY: usize = 4;

/// The last page one of each search, per user and client, for a short while. Without it a
/// later page would have to build its outside songs again, and a second build is free to come
/// back different (Last.fm reorders, a lookup times out), which would repeat or drop rows the
/// user already scrolled past.
///
/// The entry is for one user, one client, one endpoint and one music folder, because the
/// library side of the order is: two users can own different songs, and a folder narrows what
/// Navidrome returns. The client is in it because one person on two devices scrolls two lists.
/// When an entry is gone (expired, evicted, or Octo restarted) a later page builds the order
/// again, which is the best that can be done without it.
///
/// One entry holds an order per page-one size. A client can ask for page one twice at once
/// with different sizes: Feishin lists a search 50 rows at a time while counting it 500 at a
/// time, both from offset 0. With one order per entry the second page one overwrote the first,
/// and the list's next page came out of the count's order, repeating and skipping rows.
pub struct SearchSongOrderCache {
    orders: ExpiringCache<Arc<Entry>>,
    set_gate: Mutex<()>,
}

impl Default for SearchSongOrderCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchSongOrderCache {
    pub fn new() -> Self {
        SearchSongOrderCache {
            orders: ExpiringCache::new(CAPACITY),
            set_gate: Mutex::new(()),
        }
    }

    pub fn key(
        user: &str,
        client: &str,
        endpoint: &str,
        music_folder_id: Option<&str>,
        query: &str,
    ) -> String {
        // Lower-cased and trimmed the same way ExternalSearchService keys its builds, so an
        // order and the build it came from agree on which queries are the same query.
        format!(
            "{user}\n{client}\n{endpoint}\n{}\n{}",
            music_folder_id.unwrap_or(""),
            octo_core::common::dotnet::to_lower_invariant(query.trim())
        )
    }

    /// Entries kept right now. Tests read it.
    pub fn count(&self) -> usize {
        self.orders.len()
    }

    /// The order a later page at `song_offset` of `song_count` rows carries on from. Only a
    /// page one no longer than the offset fits, because the page starts after it: of those,
    /// the one asked for this same count, which is the list this page belongs to, else the
    /// latest. With none, the order is built again. A page starting inside a page one's rows
    /// is not that list's next page, whatever its size.
    pub fn get(&self, key: &str, song_count: i32, song_offset: i32) -> Option<SearchSongOrder> {
        self.orders.get(key)?.for_page(song_count, song_offset)
    }

    pub fn set(&self, key: &str, order: SearchSongOrder) {
        // Under one lock so two page ones landing together both end up in the same entry.
        let _gate = self.set_gate.lock();
        let entry = match self.orders.get(key) {
            Some(entry) => entry,
            None => {
                let entry = Arc::new(Entry::default());
                self.orders.set(key, Arc::clone(&entry), MAX_AGE, Some(IDLE));
                entry
            }
        };
        entry.add(order);
    }
}

/// One search's orders, one per page-one size, latest last.
#[derive(Default)]
struct Entry {
    orders: Mutex<Vec<SearchSongOrder>>,
}

impl Entry {
    fn add(&self, order: SearchSongOrder) {
        let mut orders = self.orders.lock();
        orders.retain(|kept| kept.page_one_count != order.page_one_count);
        orders.push(order);
        if orders.len() > ORDERS_PER_ENTRY {
            orders.remove(0);
        }
    }

    fn for_page(&self, song_count: i32, song_offset: i32) -> Option<SearchSongOrder> {
        let orders = self.orders.lock();
        orders
            .iter()
            .rev()
            .find(|o| o.page_one_count == song_count && o.page_one_count <= song_offset)
            .or_else(|| orders.iter().rev().find(|o| o.page_one_count <= song_offset))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SearchPagingTests.APageInsidePageOne_DoesNotBorrowItsOrder`: a page that starts inside
    /// page one's rows is not that list's next page, even at page one's size: it used to be
    /// placed as if it were, and repeated rows the list had.
    #[test]
    fn a_page_inside_page_one_does_not_borrow_its_order() {
        let cache = SearchSongOrderCache::new();
        let key = SearchSongOrderCache::key("alice", "Test", "rest/search3", None, "paging");
        let page_one = |count: i32| SearchSongOrder::from(Arc::new(Vec::new()), count, count, 0, &[]);
        cache.set(&key, page_one(20));
        cache.set(&key, page_one(50));

        assert_eq!(cache.get(&key, 50, 50).map(|o| o.page_one_count), Some(50));
        assert_eq!(cache.get(&key, 50, 30).map(|o| o.page_one_count), Some(20));
        assert!(cache.get(&key, 50, 10).is_none());
    }

    #[test]
    fn an_entry_keeps_one_order_per_size_and_four_at_most() {
        let cache = SearchSongOrderCache::new();
        let key = SearchSongOrderCache::key("alice", "Test", "rest/search3", Some("1"), " Paging ");
        assert_eq!(key, "alice\nTest\nrest/search3\n1\npaging");
        for count in [10, 20, 30, 40, 50, 20] {
            cache.set(
                &key,
                SearchSongOrder::from(Arc::new(Vec::new()), count, count, 0, &[]),
            );
        }
        assert_eq!(cache.count(), 1);
        // 10 fell out (oldest), 20 was replaced and is now the latest.
        assert_eq!(cache.get(&key, 10, 10).map(|o| o.page_one_count), None);
        assert_eq!(cache.get(&key, 99, 100).map(|o| o.page_one_count), Some(20));
    }

    #[test]
    fn later_externals_leave_out_songs_the_library_already_listed() {
        let song = |id: &str, artist: &str, title: &str| Song {
            id: id.into(),
            artist: artist.into(),
            title: title.into(),
            ..Default::default()
        };
        let built = Arc::new(vec![
            song("e0", "Outside Artist", "Outside Song 0"),
            song("e1", "Owned Artist", "Library Song 3"),
            song("e2", "Outside Artist", "Outside Song 2"),
        ]);
        let mut row = serde_json::Map::new();
        row.insert("artist".into(), "Owned Artist".into());
        row.insert("title".into(), "Library Song 3".into());

        // 20 asked for, 12 local / 8 external targets, one library row back: page one shows
        // min(3, 8 + 11) = 3 outside rows, so nothing is left for later.
        let order = SearchSongOrder::from(Arc::clone(&built), 20, 12, 8, &[SearchRow::Json(row.clone())]);
        assert_eq!((order.page_one_externals, order.prefix_count), (3, 1));
        assert!(order.later_externals.is_empty());
        assert!(!order.library_continues());

        // With no room for outside rows on page one, the rest is every built song but the one
        // the library already listed.
        let order = SearchSongOrder::from(built, 20, 1, 0, &[SearchRow::Json(row)]);
        let later: Vec<&str> = order.later_externals.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(later, ["e0", "e2"]);
        assert!(order.library_continues());
    }
}
