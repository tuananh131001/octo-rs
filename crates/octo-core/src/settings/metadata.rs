//! `Octo.Models.Settings.MetadataSettings` and `LyricsSaveTo` (the `Metadata` section).

use serde::{Deserialize, Serialize};

use super::text::{lower_invariant, upper_invariant, utf16_len};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct MetadataSettings {
    /// Language code sent as Accept-Language to the external metadata APIs
    /// (Deezer, Last.fm). Deezer localizes album genre names by caller IP
    /// unless told otherwise, so a server hosted in a non-English country
    /// writes localized genre tags into downloaded files. Empty lets the
    /// provider decide from the server's IP.
    pub language: String,

    /// A download that still has no album after Deezer and its own tags is filed as a single
    /// under its title, instead of joining every other album-less track in Navidrome's one
    /// "[Unknown Album]" (#50). A single filed under its title is a real release, and it gives
    /// Navidrome something to group. Never applied to a compilation, where a hundred one-track
    /// albums would be worse than the bucket.
    /// Environment variable: ALBUM_FROM_TITLE
    pub album_from_title: bool,

    /// Ask the Cover Art Archive first when a fingerprint named the MusicBrainz release the album
    /// tag describes: the right pressing, no guessing by name (#51).
    /// Environment variable: COVER_ART_ARCHIVE
    pub use_cover_art_archive: bool,

    /// Treat a cover that is not square as missing. A 16:9 cover is a video thumbnail; when
    /// nothing better turns up its centre square is used, which for a YouTube "Topic" upload is
    /// the real cover inside the letterbox. Off keeps whatever the source embedded.
    /// Environment variable: REPLACE_VIDEO_COVERS
    pub replace_video_covers: bool,

    /// Also write cover.jpg beside a download, which Navidrome reads, which survives a retag, and
    /// which covers a file the embed did not stick to. Only in the Organized layout and only in a
    /// folder the download created: Navidrome ranks cover.* above embedded art, so in a shared
    /// folder, or an album folder that was already there, one file would change every album's
    /// cover. Never replaces a cover.* or folder.* the owner put there; a cover.jpg Octo wrote
    /// itself (it carries a comment saying so) gives way to a larger one.
    /// Environment variable: COVER_FILE
    pub write_cover_file: bool,

    /// Embed the cover at the full size it was found, often 3000 px from iTunes, instead of
    /// shrinking it to [`MetadataSettings::EMBEDDED_COVER_SIDE`]. Every file of an album carries
    /// its own copy, so a full-size cover adds a few megabytes to each; cover.jpg always gets the
    /// full size either way. Also what the cover upgrade embeds.
    /// Environment variable: FULL_SIZE_COVERS
    pub embed_full_size_covers: bool,

    /// Fetch lyrics: a sidecar beside each download, and live for any song as it plays when the
    /// library has none (#52). Off by default like the rest; there is no destructive path, since
    /// an unmatched track simply has no lyrics file and a wrong one is a text file to delete.
    /// Environment variable: LYRICS_FETCH
    pub fetch_lyrics: bool,

    /// Lyrics sources, in order: song (the lyrics the song already has, in its tags or a file
    /// beside it, as Navidrome serves them), kugou (word-timed, deep catalogue, an unofficial
    /// API), lrclib (open, line-synced), netease (synced and deep on non-Western and older music,
    /// but an unofficial API, so it only runs when listed), lyricsovh (plain text). Timed beats
    /// plain, so a later source is only asked while nothing earlier had timing. Leaving a source
    /// out switches it off, except song, which cannot be switched off: left out, it is first.
    /// Environment variable: LYRICS_SOURCES
    pub lyrics_sources: String,

    /// Where lyrics Octo finds for a song are saved: beside (a .lrc or .txt next to the song, the
    /// default), inside (in the song's own tags), or both. Inside rewrites the audio file, which
    /// on a cloud mount uploads it again, and Navidrome sees it only after a scan, which Octo
    /// asks for. Octo marks what it writes either way, and never replaces lyrics it did not write.
    /// Environment variable: LYRICS_SAVE_TO
    pub save_lyrics_to: String,

    /// Word-timed lyrics beat line-timed ones from an earlier source: with this on, a source that
    /// only has line timing does not end the search, and a later one with word timing wins. Off,
    /// the first timed answer is taken, which asks fewer services per song.
    /// Environment variable: LYRICS_PREFER_WORD_TIMED
    pub prefer_word_timed_lyrics: bool,

    /// Let "Find lyrics for the library" write lyrics files beside every library song, not only
    /// the ones Octo downloaded. Off by default: a folder of rips or purchases is the owner's,
    /// and Octo does not add files to it unless asked. Existing lyrics files and lyrics embedded
    /// in a song are never replaced either way.
    /// Environment variable: LYRICS_WRITE_BESIDE_ALL
    pub write_lyrics_beside_all_songs: bool,

    /// File a song that arrived without an album under the first release of its recording
    /// (its original album), even when the file's own tags name a compilation or a later
    /// pressing it was ripped from. Off keeps the file's own album when the source tagged one.
    /// Environment variable: PREFER_ORIGINAL_ALBUM
    pub prefer_original_album: bool,

    /// The year a song shows is its recording's first release, not the pressing it was matched
    /// to. A 2011 remaster of a 1991 album reads 1991. The pressing's own date stays in the
    /// download's tag report. Off writes the pressing's date.
    /// Environment variable: YEAR_FROM_ORIGINAL_RELEASE
    pub year_from_original_release: bool,

    /// Countries whose pressings win a tie, in order, as two-letter codes ("US, XW, GB").
    /// Empty means no preference.
    /// Environment variable: PREFERRED_COUNTRIES
    pub preferred_countries: String,

    /// Ask the music database for the chosen release's label, catalogue number, barcode, status
    /// and track ids (one request per download, about a second), and search it by name when
    /// the fingerprint named nothing. Needs no key. Off tags from the fingerprint and the
    /// catalog alone.
    /// Environment variable: RELEASE_DETAILS_LOOKUP
    pub release_details_lookup: bool,

    /// Measure each download's loudness and write ReplayGain track tags, which both Octo apps
    /// and most players use to even out volume. Decodes the whole file once, beside the
    /// lookups, so it rarely adds time.
    /// Environment variable: REPLAYGAIN
    pub replay_gain: bool,

    /// How long the loudness measurement may take before the download goes on without it.
    /// A FLAC on local disk takes a few seconds; a slow mount can take far longer.
    /// Environment variable: REPLAYGAIN_TIMEOUT_SECONDS
    pub replay_gain_timeout_seconds: i32,

    /// Rehearse the release matching: work out what every download would be tagged as and show
    /// it in Fetched songs, but write only what Octo wrote before. For trying the matching on
    /// real downloads before trusting it.
    /// Environment variable: TAG_REHEARSAL
    pub tag_rehearsal: bool,
}

impl Default for MetadataSettings {
    fn default() -> Self {
        Self {
            language: "en".to_string(),
            album_from_title: true,
            use_cover_art_archive: true,
            replace_video_covers: true,
            write_cover_file: true,
            embed_full_size_covers: false,
            fetch_lyrics: false,
            lyrics_sources: Self::DEFAULT_LYRICS_SOURCES.to_string(),
            save_lyrics_to: LyricsSaveTo::BESIDE.to_string(),
            prefer_word_timed_lyrics: true,
            write_lyrics_beside_all_songs: false,
            prefer_original_album: true,
            year_from_original_release: true,
            preferred_countries: String::new(),
            release_details_lookup: true,
            replay_gain: true,
            replay_gain_timeout_seconds: 45,
            tag_rehearsal: false,
        }
    }
}

impl MetadataSettings {
    /// The longest side of an embedded cover when `embed_full_size_covers` is off: sharp
    /// across a phone's whole screen, a few hundred kilobytes.
    pub const EMBEDDED_COVER_SIDE: i32 = 1500;

    pub const DEFAULT_LYRICS_SOURCES: &'static str = "song,kugou,lrclib,lyricsovh";

    /// The song's own lyrics as a place in the order: in its tags or a file beside it.
    pub const SONG_LYRICS_SOURCE: &'static str = "song";

    pub const KNOWN_LYRICS_SOURCES: [&'static str; 5] = [
        Self::SONG_LYRICS_SOURCE,
        "kugou",
        "lrclib",
        "netease",
        "lyricsovh",
    ];

    /// The sources in order, the song's own always among them (first when the saved
    /// order leaves it out, as every order saved before it was a choice does).
    pub fn effective_lyrics_sources(&self) -> Vec<String> {
        let mut listed: Vec<String> = Vec::new();
        for source in split_trimmed(&self.lyrics_sources) {
            let source = lower_invariant(source);
            if Self::KNOWN_LYRICS_SOURCES.contains(&source.as_str()) && !listed.contains(&source) {
                listed.push(source);
            }
        }
        if !listed.iter().any(|s| s == Self::SONG_LYRICS_SOURCE) {
            listed.insert(0, Self::SONG_LYRICS_SOURCE.to_string());
        }
        listed
    }

    /// Whether found lyrics go in a file beside the song.
    pub fn saves_lyrics_beside(&self) -> bool {
        LyricsSaveTo::normalize(Some(&self.save_lyrics_to)) != LyricsSaveTo::INSIDE
    }

    /// Whether found lyrics go in the song's own tags.
    pub fn saves_lyrics_inside(&self) -> bool {
        LyricsSaveTo::normalize(Some(&self.save_lyrics_to)) != LyricsSaveTo::BESIDE
    }

    pub fn effective_preferred_countries(&self) -> Vec<String> {
        let mut codes: Vec<String> = Vec::new();
        for code in split_trimmed(&self.preferred_countries) {
            let code = upper_invariant(code);
            if utf16_len(&code) == 2 && !codes.contains(&code) {
                codes.push(code);
            }
        }
        codes
    }

    pub fn effective_replay_gain_timeout_seconds(&self) -> i32 {
        self.replay_gain_timeout_seconds.clamp(10, 300)
    }
}

/// `Split(',', RemoveEmptyEntries | TrimEntries)`.
fn split_trimmed(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// The places found lyrics can be saved to: [`MetadataSettings::save_lyrics_to`].
pub struct LyricsSaveTo;

impl LyricsSaveTo {
    pub const BESIDE: &'static str = "beside";
    pub const INSIDE: &'static str = "inside";
    pub const BOTH: &'static str = "both";

    /// One of the three; anything else is beside, the one that never touches a song.
    pub fn normalize(value: Option<&str>) -> &'static str {
        match lower_invariant(value.unwrap_or("").trim()).as_str() {
            Self::INSIDE => Self::INSIDE,
            Self::BOTH => Self::BOTH,
            _ => Self::BESIDE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // LyricsSongSourceTests.Order_SavedWithoutTheSong_PutsItFirst
    #[test]
    fn order_saved_without_the_song_puts_it_first() {
        let with = |sources: &str| MetadataSettings {
            lyrics_sources: sources.into(),
            ..Default::default()
        };
        assert_eq!(
            with("kugou,lrclib").effective_lyrics_sources(),
            ["song", "kugou", "lrclib"]
        );
        assert_eq!(
            with("kugou,song,lrclib").effective_lyrics_sources(),
            ["kugou", "song", "lrclib"]
        );
        assert_eq!(
            MetadataSettings::default().effective_lyrics_sources(),
            ["song", "kugou", "lrclib", "lyricsovh"]
        );
    }

    // LyricsSongSourceTests.SaveTo_AnythingUnknown_IsBeside
    #[test]
    fn save_to_anything_unknown_is_beside() {
        assert_eq!(LyricsSaveTo::normalize(Some("sideways")), LyricsSaveTo::BESIDE);
        assert_eq!(LyricsSaveTo::normalize(Some(" INSIDE ")), LyricsSaveTo::INSIDE);
        assert!(MetadataSettings::default().saves_lyrics_beside());
        assert!(!MetadataSettings::default().saves_lyrics_inside());
    }

    #[test]
    fn lyrics_sources_drop_unknown_and_duplicate_entries() {
        let s = MetadataSettings {
            lyrics_sources: " LRCLIB , nope,,lrclib, NetEase".into(),
            ..Default::default()
        };
        assert_eq!(s.effective_lyrics_sources(), ["song", "lrclib", "netease"]);
    }

    #[test]
    fn preferred_countries_keep_two_letter_codes_in_order() {
        let s = MetadataSettings {
            preferred_countries: "us, XW ,gbr,,us, gb".into(),
            ..Default::default()
        };
        assert_eq!(s.effective_preferred_countries(), ["US", "XW", "GB"]);
    }

    #[test]
    fn replay_gain_timeout_is_clamped() {
        let s = MetadataSettings {
            replay_gain_timeout_seconds: 1,
            ..Default::default()
        };
        assert_eq!(s.effective_replay_gain_timeout_seconds(), 10);
    }
}
