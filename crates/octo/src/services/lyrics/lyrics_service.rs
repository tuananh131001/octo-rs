//! Port of `Services/Lyrics/LyricsService.cs`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::is_null_or_white_space;
use octo_core::lyrics::{ILyricsSource, LyricsLookup, LyricsQuery, LyricsResult, LyricsTiming};
use octo_core::settings::{MetadataSettings, SettingsStore};
use tokio_util::sync::CancellationToken;

use super::kugou_lyrics_source::KugouLyricsSource;
use super::lrclib_lyrics_source::LrclibLyricsSource;
use super::lyrics_ovh_lyrics_source::LyricsOvhLyricsSource;
use super::memory_cache::MemoryCache;
use super::netease_lyrics_source::NeteaseLyricsSource;

/// Lyrics from the sources LYRICS_SOURCES names, in that order (#52). "song" there is the
/// lyrics the song already has, which the caller passes in, so they rank like any source. Word
/// timing beats line timing beats plain text. A synced answer ends the search, unless "prefer
/// word-timed lyrics" is on and it has only line timing: then later sources are still asked
/// for word timing, and the line-timed answer is kept in case none has it. A plain answer is
/// always kept in case something timed turns up.
pub struct LyricsService {
    sources: Vec<Arc<dyn ILyricsSource>>,
    settings: Arc<SettingsStore>,
    cache: MemoryCache<LyricsLookup>,
}

impl LyricsService {
    const HIT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
    const MISS_TTL: Duration = Duration::from_secs(30 * 60);

    pub fn new(sources: Vec<Arc<dyn ILyricsSource>>, settings: Arc<SettingsStore>) -> Self {
        Self {
            sources,
            settings,
            cache: MemoryCache::new(512),
        }
    }

    /// The four sources `Program.cs` registered, in its order: KuGou on the `kugou` client,
    /// the others on the shared `lyrics` client.
    pub fn default_sources(
        lyrics_client: reqwest::Client,
        kugou_client: reqwest::Client,
    ) -> Vec<Arc<dyn ILyricsSource>> {
        vec![
            Arc::new(KugouLyricsSource::new(kugou_client)),
            Arc::new(LrclibLyricsSource::new(lyrics_client.clone())),
            Arc::new(NeteaseLyricsSource::new(lyrics_client.clone())),
            Arc::new(LyricsOvhLyricsSource::new(lyrics_client)),
        ]
    }

    /// The sources in the order they are asked, only those that are on.
    pub fn enabled(&self) -> Vec<Arc<dyn ILyricsSource>> {
        self.settings
            .current()
            .metadata
            .effective_lyrics_sources()
            .iter()
            .filter_map(|name| self.source(name))
            .collect()
    }

    pub fn source(&self, key: &str) -> Option<Arc<dyn ILyricsSource>> {
        self.sources.iter().find(|source| source.key() == key).cloned()
    }

    /// The best lyrics the sources have. When `ct` runs out part way (a client waiting has a
    /// budget), the best answer found so far is returned rather than nothing, and remembered
    /// only briefly. `songs_own` is how the lyrics the song already has are timed, None when it
    /// has none; at "song"'s place in the order they answer as [`LyricsResult::songs_own`], and
    /// the caller serves its own copy.
    pub async fn find(
        &self,
        query: &LyricsQuery,
        ct: &CancellationToken,
        songs_own: LyricsTiming,
    ) -> LyricsLookup {
        if is_null_or_white_space(Some(&query.artist)) || is_null_or_white_space(Some(&query.title)) {
            return LyricsLookup::miss();
        }

        let settings = self.settings.current();
        let order = settings.metadata.effective_lyrics_sources();
        let prefer_words = settings.metadata.prefer_word_timed_lyrics;
        let key = format!(
            "{}|{}|{}|{}|{:?}",
            SongIdentity::match_key(&query.artist, &query.title),
            query.duration_seconds.map(|d| d.to_string()).unwrap_or_default(),
            order.join(","),
            if prefer_words { "True" } else { "False" },
            songs_own
        );
        if let Some(cached) = self.cache.get(&key) {
            return cached;
        }

        let mut best: Option<LyricsResult> = None;
        let mut transient = false;
        for name in &order {
            if name == MetadataSettings::SONG_LYRICS_SOURCE {
                if songs_own == LyricsTiming::None {
                    continue;
                }
                let own = LyricsResult::songs_own(songs_own);
                if songs_own == LyricsTiming::Word || (songs_own == LyricsTiming::Line && !prefer_words) {
                    return self.remember(&key, LyricsLookup::new(Some(own), false), Self::HIT_TTL);
                }
                if best.as_ref().is_none_or(|best| own.timing() > best.timing()) {
                    best = Some(own);
                }
                continue;
            }
            let Some(source) = self.source(name) else {
                continue;
            };
            // The sources never throw: a failure, a cancellation or an odd answer is a
            // transient lookup, which is what the C# made of an exception.
            let lookup = source.find(query, ct).await;
            if ct.is_cancelled() && lookup.result.is_none() {
                transient = true;
                break;
            }

            if lookup.transient {
                transient = true;
                continue;
            }
            let Some(result) = lookup.result else {
                continue;
            };
            if result.instrumental
                || result.timing() == LyricsTiming::Word
                || (result.timing() == LyricsTiming::Line && !prefer_words)
            {
                return self.remember(&key, LyricsLookup::new(Some(result), false), Self::HIT_TTL);
            }
            if best.as_ref().is_none_or(|best| result.timing() > best.timing()) {
                best = Some(result);
            }
        }

        // An answer found while a better source could not be asked is kept only briefly, so the
        // better one gets another chance soon.
        if let Some(best) = best {
            let ttl = if transient { Self::MISS_TTL } else { Self::HIT_TTL };
            return self.remember(&key, LyricsLookup::new(Some(best), false), ttl);
        }
        if transient {
            return LyricsLookup::failed();
        }
        self.remember(&key, LyricsLookup::miss(), Self::MISS_TTL)
    }

    fn remember(&self, key: &str, lookup: LyricsLookup, ttl: Duration) -> LyricsLookup {
        self.cache.set(key, lookup.clone(), ttl);
        lookup
    }

    /// Forget what was found, after the source order or a pin changed.
    pub fn clear(&self) {
        self.cache.clear();
    }
}

#[cfg(test)]
#[path = "lyrics_service_tests.rs"]
mod tests;
