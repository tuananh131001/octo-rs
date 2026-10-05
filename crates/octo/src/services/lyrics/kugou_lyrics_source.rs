//! Port of `Services/Lyrics/KugouLyricsSource.cs`.

use std::io::Read;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use flate2::read::ZlibDecoder;
use octo_core::common::dotnet::{is_null_or_white_space, utf16_len};
use octo_core::lyrics::{
    ILyricsSource, LyricLine, LyricWord, LyricsCandidate, LyricsIdentity, LyricsLookup, LyricsQuery,
    LyricsResult, LyricsSearch, LyricsText,
};
use parking_lot::Mutex;
use regex::Regex;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::lyrics_http::{
    ShapeError, escape_data_string, from_base64, prop, retry_after_delta, round_to_int, str_prop,
    try_get_i32, try_get_i64,
};
use super::web_utility::html_decode;

/// The key every KRC file is XORed with, public since 2012.
const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

// The C# patterns used `\d`, which .NET also matched in other scripts' digits before
// `long.Parse` threw; ASCII digits only here (known-diffs.md).
static KRC_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[([0-9]+),([0-9]+)\](.*)$").expect("a fixed pattern compiles"));
/// A word's offset can be negative (a credit timed before its line).
static KRC_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(-?[0-9]+),(-?[0-9]+),-?[0-9]+>").expect("a fixed pattern compiles"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("a fixed pattern compiles"));

/// What KuGou shows for a track with no words: "pure music, please enjoy".
const INSTRUMENTAL_MARK: &str = "纯音乐";

/// KuGou Music: the one source here with word timing for most songs, in its own KRC format, so
/// it drives the Octo app's word-by-word lyrics. Its API is undocumented, keyless and
/// unlicensed, the same standing as NetEase's, and it can change or vanish without notice; so
/// every failure is a quiet miss or a cool-down, never an exception, and dropping "kugou" from
/// LYRICS_SOURCES switches it off.
///
/// Three public endpoints, all unsigned:
///   lyrics.kugou.com/search     lyric entries for "artist - title" (and a length, or a song hash)
///   mobileservice.kugou.com     the song catalogue, asked only when the lyric search has no
///                               entry for this song, for the song's hash
///   lyrics.kugou.com/download   one entry's lyric: KRC (fmt=krc) or LRC (fmt=lrc), base64
/// KRC is "krc1", then zlib data XORed with a fixed 16-byte key, giving text with a
/// [lineStart,lineLength] tag per line and a <offset,length,0> tag per word.
pub struct KugouLyricsSource {
    client: reqwest::Client,
    /// `https://lyrics.kugou.com`: the lyric search and download.
    lyrics_url: String,
    /// `https://mobileservice.kugou.com`: the song catalogue.
    catalogue_url: String,
    /// The one-at-a-time gate, holding when the last request went out.
    gate: tokio::sync::Mutex<Option<Instant>>,
    cool_down_until: Mutex<Option<Instant>>,
    failures: AtomicU32,
}

/// One answer: the JSON, or none (a 404), and whether it was "not now".
struct Answer {
    json: Option<Value>,
    transient: bool,
}

impl Answer {
    fn not_now() -> Self {
        Self {
            json: None,
            transient: true,
        }
    }
}

/// A song in KuGou's catalogue.
struct Song {
    hash: String,
    title: String,
    artist: String,
    album: Option<String>,
    seconds: i32,
}

impl KugouLyricsSource {
    pub const CLIENT_NAME: &'static str = super::lyrics_http::KUGOU_CLIENT_NAME;
    pub const DEFAULT_LYRICS_URL: &'static str = "https://lyrics.kugou.com";
    pub const DEFAULT_CATALOGUE_URL: &'static str = "https://mobileservice.kugou.com";

    const MINIMUM_GAP: Duration = Duration::from_millis(300);
    const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

    /// After this many failures in a row KuGou is left alone for a while, so a dead or changed
    /// API costs playback nothing but one quick check every few minutes.
    const FAILURES_BEFORE_BREAK: u32 = 5;
    const BREAK_LENGTH: Duration = Duration::from_secs(5 * 60);

    /// The source over the `kugou` client ([`super::lyrics_http::kugou_http_client`]).
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_base_urls(client, Self::DEFAULT_LYRICS_URL, Self::DEFAULT_CATALOGUE_URL)
    }

    /// The same against other addresses, for tests.
    pub fn with_base_urls(client: reqwest::Client, lyrics_url: &str, catalogue_url: &str) -> Self {
        Self {
            client,
            lyrics_url: lyrics_url.trim_end_matches('/').to_string(),
            catalogue_url: catalogue_url.trim_end_matches('/').to_string(),
            gate: tokio::sync::Mutex::new(None),
            cool_down_until: Mutex::new(None),
            failures: AtomicU32::new(0),
        }
    }

    /// Until when the service asked to be left alone, for a caller that would rather wait than
    /// give up.
    pub fn cool_down_until(&self) -> Option<Instant> {
        *self.cool_down_until.lock()
    }

    /// The lyrics of the best entry in a search already made that is this song.
    pub async fn find_in(
        &self,
        query: &LyricsQuery,
        search: &LyricsSearch,
        ct: &CancellationToken,
    ) -> LyricsLookup {
        if search.transient && search.candidates.is_empty() {
            return LyricsLookup::failed();
        }

        let mut transient = search.transient;
        let mut matching: Vec<&LyricsCandidate> = search
            .candidates
            .iter()
            .filter(|candidate| Self::is_this_song(candidate, query))
            .collect();
        // OrderBy is stable.
        matching.sort_by(|a, b| {
            Self::distance(a, query)
                .partial_cmp(&Self::distance(b, query))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for candidate in matching.into_iter().take(2) {
            let lookup = self.fetch_for(&candidate.id, Some(query), ct).await;
            if lookup.transient {
                transient = true;
                continue;
            }
            let Some(mut result) = lookup.result else {
                continue;
            };
            result.candidate_id = Some(candidate.candidate_id());
            result.doubt =
                LyricsIdentity::doubt(query.duration_seconds, candidate.duration_seconds.map(f64::from));
            return LyricsLookup::new(Some(result), false);
        }
        if transient {
            LyricsLookup::failed()
        } else {
            LyricsLookup::miss()
        }
    }

    pub fn is_this_song(candidate: &LyricsCandidate, query: &LyricsQuery) -> bool {
        let credits = Self::credits(&candidate.artist);
        LyricsIdentity::same_song(
            &query.title,
            &query.artist,
            Some(&candidate.title),
            Some(&candidate.artist),
            Some(credits.as_slice()),
        ) && LyricsIdentity::length_fits(query.duration_seconds, candidate.duration_seconds.map(f64::from))
    }

    fn distance(candidate: &LyricsCandidate, query: &LyricsQuery) -> f64 {
        match (
            query.duration_seconds.filter(|d| *d > 0),
            candidate.duration_seconds.filter(|d| *d > 0),
        ) {
            (Some(want), Some(got)) => f64::from((got - want).abs()),
            _ => 0.0,
        }
    }

    /// KuGou joins a song's artists with "、".
    fn credits(artist: &str) -> Vec<&str> {
        artist
            .split(['、', ',', '&', '/'])
            .map(str::trim)
            .filter(|credit| !credit.is_empty())
            .collect()
    }

    /// The lyric entries for "artist - title", then for the same song written the ways
    /// `SongIdentity.QueryVariants` gives ("suicideboys - SUICIDE" for "$uicideboy$ -
    /// $UICIDE"), until one of them is this song. When none is, the song catalogue is asked too,
    /// and the lyric entries for each catalogue song that IS this song are added under that
    /// song's name and album.
    async fn search_inner(
        &self,
        query: &LyricsQuery,
        ct: &CancellationToken,
    ) -> Result<LyricsSearch, ShapeError> {
        let searches = LyricsIdentity::searches(query, 3);
        let mut candidates: Vec<LyricsCandidate> = Vec::new();
        for search in &searches {
            let mut url = format!(
                "{}/search?ver=1&man=yes&client=pc&keyword={}",
                self.lyrics_url,
                escape_data_string(&format!("{} - {}", search.artist, search.title))
            );
            if let Some(duration) = query.duration_seconds.filter(|d| (1..=3600).contains(d)) {
                url.push_str(&format!("&duration={}", i64::from(duration) * 1000));
            }

            let direct = self.get_json(&url, ct).await;
            if direct.transient {
                return Ok(if candidates.is_empty() {
                    LyricsSearch::failed()
                } else {
                    LyricsSearch::new(Self::distinct(candidates), true)
                });
            }
            // In front: the list is cut to a dozen, and an earlier search's entries were not the song.
            let entries = Self::read_lyric_entries(direct.json.as_ref(), None, true)?;
            candidates.splice(0..0, entries);
            if candidates
                .iter()
                .any(|candidate| Self::is_this_song(candidate, query))
            {
                return Ok(LyricsSearch::new(Self::distinct(candidates), false));
            }
        }

        let catalogue = self
            .get_json(
                &format!(
                    "{}/api/v3/search/song?format=json&page=1&pagesize=10&showtype=1&keyword={}",
                    self.catalogue_url,
                    escape_data_string(&searches[0].text())
                ),
                ct,
            )
            .await;
        if catalogue.transient {
            return Ok(LyricsSearch::new(Self::distinct(candidates), true));
        }

        let songs = Self::read_songs(catalogue.json.as_ref())?;
        // The catalogue names some artists its own way ("Ye (侃爷)" for Kanye West), so a song
        // with the right title and length is asked about even when its artist reads otherwise;
        // its lyric entries then carry the lyric's own credit, and the identity check decides.
        let mut transient = false;
        let mut found: Vec<LyricsCandidate> = Vec::new();
        let same_artist = |song: &Song| {
            LyricsIdentity::same_artist(
                &query.artist,
                Some(&song.artist),
                Some(Self::credits(&song.artist).as_slice()),
            )
        };
        let mut likely: Vec<&Song> = songs
            .iter()
            .filter(|song| {
                LyricsIdentity::same_title(&query.title, Some(&song.title))
                    && LyricsIdentity::length_fits(query.duration_seconds, Some(f64::from(song.seconds)))
            })
            .collect();
        // OrderByDescending is stable: the named artist's songs first.
        likely.sort_by_key(|song| std::cmp::Reverse(same_artist(song)));
        for song in likely.into_iter().take(2) {
            let mut url = format!(
                "{}/search?ver=1&man=yes&client=pc&hash={}",
                self.lyrics_url,
                escape_data_string(&song.hash)
            );
            if song.seconds > 0 {
                url.push_str(&format!("&duration={}", i64::from(song.seconds) * 1000));
            }
            let by_hash = self.get_json(&url, ct).await;
            if by_hash.transient {
                transient = true;
                continue;
            }
            // A few per song, so the entries of the likelier song (named artist first) are not
            // crowded out of the list by the other's.
            let named = same_artist(song);
            let entries = Self::read_lyric_entries(by_hash.json.as_ref(), Some(song), named)?;
            found.extend(entries.into_iter().take(4));
        }
        // In front: these may be the song, the entries before them were not, and the list is
        // cut to a dozen.
        candidates.splice(0..0, found);
        Ok(LyricsSearch::new(Self::distinct(candidates), transient))
    }

    fn distinct(candidates: Vec<LyricsCandidate>) -> Vec<LyricsCandidate> {
        let mut seen = std::collections::HashSet::new();
        candidates
            .into_iter()
            .filter(|candidate| seen.insert(candidate.id.clone()))
            .take(12)
            .collect()
    }

    fn read_songs(root: Option<&Value>) -> Result<Vec<Song>, ShapeError> {
        let Some(element) = root else {
            return Ok(Vec::new());
        };
        let Some(data) = prop(element, "data")? else {
            return Ok(Vec::new());
        };
        let Some(Value::Array(info)) = prop(data, "info")? else {
            return Ok(Vec::new());
        };
        let mut songs = Vec::new();
        for song in info {
            let seconds = match prop(song, "duration")? {
                Some(duration) => try_get_i32(duration)?.unwrap_or(0),
                None => 0,
            };
            let song = Song {
                hash: str_prop(song, "hash")?.unwrap_or("").to_string(),
                title: str_prop(song, "songname")?.unwrap_or("").to_string(),
                artist: str_prop(song, "singername")?.unwrap_or("").to_string(),
                album: str_prop(song, "album_name")?.map(str::to_string),
                seconds,
            };
            if !song.hash.is_empty() {
                songs.push(song);
            }
        }
        Ok(songs)
    }

    /// Lyric entries, titled after the catalogue song they were found through when there is
    /// one, and credited to its artist when that artist is the one asked for; otherwise to the
    /// lyric's own singer, so the identity check still judges the artist.
    fn read_lyric_entries(
        root: Option<&Value>,
        song: Option<&Song>,
        song_artist: bool,
    ) -> Result<Vec<LyricsCandidate>, ShapeError> {
        let Some(element) = root else {
            return Ok(Vec::new());
        };
        let Some(Value::Array(list)) = prop(element, "candidates")? else {
            return Ok(Vec::new());
        };
        let mut entries = Vec::new();
        for entry in list {
            let (Some(id), Some(key)) = (str_prop(entry, "id")?, str_prop(entry, "accesskey")?) else {
                continue;
            };
            if id.is_empty() || key.is_empty() {
                continue;
            }
            let seconds = match prop(entry, "duration")? {
                Some(duration) => try_get_i64(duration)?
                    .filter(|ms| *ms > 0)
                    .map(|ms| round_to_int(ms as f64 / 1000.0)),
                None => None,
            };
            let title = match song {
                Some(song) => song.title.clone(),
                None => str_prop(entry, "song")?.unwrap_or("").to_string(),
            };
            let artist = match song.filter(|_| song_artist) {
                Some(song) => song.artist.clone(),
                None => str_prop(entry, "singer")?.unwrap_or("").to_string(),
            };
            entries.push(LyricsCandidate::new(
                "kugou",
                format!("{id}.{key}"),
                title,
                artist,
                song.and_then(|song| song.album.clone()),
                seconds.or(song.map(|song| song.seconds).filter(|s| *s > 0)),
            ));
        }
        Ok(entries)
    }

    /// One entry's lyric: its KRC, word-timed, or its LRC when it has no KRC. The song, when
    /// known, lets a first line that only names it ("Artist - Title") be dropped.
    pub async fn fetch_for(
        &self,
        id: &str,
        song: Option<&LyricsQuery>,
        ct: &CancellationToken,
    ) -> LyricsLookup {
        let Some(dot) = id.find('.').filter(|dot| *dot > 0 && *dot != id.len() - 1) else {
            return LyricsLookup::miss();
        };
        let download = format!(
            "{}/download?ver=1&client=pc&id={}&accesskey={}&charset=utf8&fmt=",
            self.lyrics_url,
            escape_data_string(&id[..dot]),
            escape_data_string(&id[dot + 1..])
        );

        let krc = self.get_json(&format!("{download}krc"), ct).await;
        if krc.transient {
            return LyricsLookup::failed();
        }
        let Ok(mut text) = Self::content(krc.json.as_ref()) else {
            return LyricsLookup::failed();
        };
        let mut lines = text.as_deref().map(Self::parse_krc);

        if lines.as_ref().is_none_or(Vec::is_empty) {
            let lrc = self.get_json(&format!("{download}lrc"), ct).await;
            if lrc.transient {
                return LyricsLookup::failed();
            }
            let Ok(content) = Self::content(lrc.json.as_ref()) else {
                return LyricsLookup::failed();
            };
            text = content;
            if is_null_or_white_space(text.as_deref()) {
                return LyricsLookup::miss();
            }
            let decoded = html_decode(text.as_deref().unwrap_or(""));
            lines = LyricsText::has_timestamps(Some(&decoded)).then(|| LyricsText::parse_lrc(&decoded));
            if lines.is_none() {
                return Self::plain(&decoded);
            }
        }

        Self::to_result(&Self::clean(&lines.unwrap_or_default(), song))
    }

    fn plain(text: &str) -> LyricsLookup {
        let clean = text.replace("\r\n", "\n").trim().to_string();
        if clean.contains(INSTRUMENTAL_MARK) && utf16_len(&clean) < 40 {
            return LyricsLookup::new(Some(LyricsResult::new("KuGou", None, None, true)), false);
        }
        LyricsLookup::new(Some(LyricsResult::new("KuGou", None, Some(clean), false)), false)
    }

    fn to_result(lines: &[LyricLine]) -> LyricsLookup {
        let sung: Vec<&LyricLine> = lines.iter().filter(|line| !line.text.is_empty()).collect();
        if sung.is_empty() {
            return LyricsLookup::miss();
        }
        if sung.len() <= 2 && sung.iter().all(|line| line.text.contains(INSTRUMENTAL_MARK)) {
            return LyricsLookup::new(Some(LyricsResult::new("KuGou", None, None, true)), false);
        }
        let synced = LyricsText::strip_credits(&LyricsText::write_lrc(lines));
        if is_null_or_white_space(Some(&synced)) {
            LyricsLookup::miss()
        } else {
            LyricsLookup::new(Some(LyricsResult::new("KuGou", Some(synced), None, false)), false)
        }
    }

    /// KuGou's own furniture, taken out of the lines:
    /// - a first line that only names the song, "Artist - Title" or "Title (Explicit) - Artist";
    /// - who sings next, "Drake：" as a line of its own (dropped) or ahead of the words
    ///   ("Kanye West：Real friends", where the words stay). A label that is a contributor role
    ///   ("Written by：Drake") takes its whole line with it.
    pub fn clean(lines: &[LyricLine], song: Option<&LyricsQuery>) -> Vec<LyricLine> {
        let mut kept: Vec<LyricLine> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            // The name line sits among the first few, before or after the credits; it is only
            // looked for until the first line with words in it is kept.
            if let Some(song) = song
                && index < 6
                && !kept.iter().any(|sung| !sung.text.is_empty())
                && Self::names_song(&line.text, song)
            {
                continue;
            }

            let Some((label, cut)) = Self::speaker(&line.text) else {
                kept.push(line.clone());
                continue;
            };
            if LyricsText::is_credit_label(label) {
                continue;
            }
            if cut >= line.text.len() {
                continue;
            }
            let words = line
                .words
                .iter()
                .filter(|word| word.to > cut)
                .map(|word| LyricWord {
                    from: word.from.max(cut) - cut,
                    to: word.to - cut,
                    ..*word
                })
                .filter(|word| word.to > word.from)
                .collect();
            kept.push(LyricLine {
                text: line.text[cut..].to_string(),
                words,
                ..line.clone()
            });
        }
        kept
    }

    /// "Singer：", KuGou's own way of saying who sings next, always with a full-width colon,
    /// which an English lyric never uses: `^([^：]{1,60})：\s*`, the label counted in UTF-16
    /// units as .NET counted it. The label, and where the words after it start.
    fn speaker(text: &str) -> Option<(&str, usize)> {
        let colon = text.find('：')?;
        let label = &text[..colon];
        if !(1..=60).contains(&utf16_len(label)) {
            return None;
        }
        let after = colon + '：'.len_utf8();
        let spaces = text[after..]
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map_or(text.len() - after, |(at, _)| at);
        Some((label, after + spaces))
    }

    fn names_song(text: &str, song: &LyricsQuery) -> bool {
        let Some(dash) = text.rfind(" - ").filter(|dash| *dash > 0) else {
            return false;
        };
        let before = LyricsIdentity::title_key(&text[..dash]);
        let after = &text[dash + 3..];
        let title = LyricsIdentity::title_key(&song.title);
        if title.is_empty() {
            return false;
        }
        // "Artist - Title", or "Title - whoever", the title whole
        if LyricsIdentity::title_key(after) == title || before == title {
            return true;
        }
        // "Title (Explicit) - Artist/Guest"
        let lead = LyricsIdentity::lead_artist(&song.artist);
        before.starts_with(&title)
            && after
                .split(['/', '、', ','])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .any(|name| LyricsIdentity::lead_artist(name) == lead)
    }

    /// The base64 lyric in a download answer, decoded: KRC when it carries the KRC header,
    /// otherwise the text as it came (an LRC or plain lyric).
    fn content(root: Option<&Value>) -> Result<Option<String>, ShapeError> {
        let Some(element) = root else {
            return Ok(None);
        };
        let Some(content) = str_prop(element, "content")?.filter(|content| !content.is_empty()) else {
            return Ok(None);
        };
        Ok(from_base64(content).map(|bytes| {
            Self::decode_krc(&bytes).unwrap_or_else(|| {
                String::from_utf8_lossy(&bytes)
                    .trim_start_matches('\u{FEFF}')
                    .to_string()
            })
        }))
    }

    /// KRC to text: past the "krc1" header, XOR with the key, then inflate. None when the bytes
    /// are not a KRC file or do not inflate.
    pub fn decode_krc(data: &[u8]) -> Option<String> {
        if data.len() <= 4 || &data[..4] != b"krc1" {
            return None;
        }
        let body: Vec<u8> = data[4..]
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ KRC_KEY[index % KRC_KEY.len()])
            .collect();
        let mut inflated = Vec::new();
        ZlibDecoder::new(body.as_slice())
            .read_to_end(&mut inflated)
            .ok()?;
        // StreamReader: a byte order mark is not text, and bytes that are not UTF-8 read as U+FFFD.
        let text = String::from_utf8_lossy(&inflated);
        Some(text.trim_start_matches('\u{FEFF}').to_string())
    }

    /// KRC text as timed lines with timed words. A line is "[start,length]" in milliseconds
    /// from the song's start, and each word "<offset,length,0>text" with the offset from the
    /// line's start. Tag lines such as [ti:] and [language:] are skipped. Words are joined as
    /// KuGou spaced them, with runs of spaces made one.
    pub fn parse_krc(krc: &str) -> Vec<LyricLine> {
        let mut lines = Vec::new();
        for raw in krc.replace("\r\n", "\n").split('\n') {
            if let Some(line) = Self::parse_krc_line(raw.trim()) {
                lines.push(line);
            }
        }
        // OrderBy is stable, as sort_by_key is.
        lines.sort_by_key(|line| line.start_ms);
        lines
    }

    /// One KRC line. None when it is not one (or, where C# threw from `long.Parse`, when a
    /// number does not fit).
    fn parse_krc_line(raw: &str) -> Option<LyricLine> {
        let found = KRC_LINE.captures(raw)?;
        let start: i64 = found[1].parse().ok()?;
        let length: i64 = found[2].parse().ok()?;
        let body = found.get(3).map_or("", |body| body.as_str());
        let line_end = (length > 0).then(|| start + length);

        let tags: Vec<regex::Captures> = KRC_WORD.captures_iter(body).collect();
        if tags.is_empty() {
            let mut line = LyricLine::new(start, html_decode(body).trim());
            line.end_ms = line_end;
            return Some(line);
        }

        let mut text = String::new();
        let mut words: Vec<LyricWord> = Vec::new();
        for (index, tag) in tags.iter().enumerate() {
            let whole = tag.get(0).expect("a match has its whole");
            let from = whole.end();
            let to = tags
                .get(index + 1)
                .map_or(body.len(), |next| next.get(0).map_or(body.len(), |m| m.start()));
            let decoded = html_decode(&body[from..to]);
            let mut piece = SPACES.replace_all(&decoded, " ").into_owned();
            if text.is_empty() || text.ends_with(' ') {
                piece = piece.trim_start().to_string();
            }
            if piece.is_empty() {
                continue;
            }

            let word_start = start + tag[1].parse::<i64>().ok()?;
            let word_length: i64 = tag[2].parse().ok()?;
            let at = text.len();
            text.push_str(&piece);
            if !is_null_or_white_space(Some(&piece)) {
                words.push(LyricWord::new(
                    word_start,
                    (word_length > 0).then(|| word_start + word_length),
                    at,
                    text.len(),
                ));
            }
        }

        let line = text.trim_end().to_string();
        let placed: Vec<LyricWord> = words
            .into_iter()
            .map(|word| LyricWord {
                to: word.to.min(line.len()),
                ..word
            })
            .filter(|word| word.to > word.from)
            .collect();
        let end_ms = match placed.last() {
            Some(last) => last.end_ms.or(line_end),
            None => line_end,
        };
        let mut parsed = LyricLine::new(start, line);
        parsed.words = placed;
        parsed.end_ms = end_ms;
        Some(parsed)
    }

    /// One request, one at a time with a short gap. A 429 or 503 cools KuGou down for as long
    /// as it asks; five failures in a row (timeouts, refused connections, pages that are not
    /// JSON) open a five-minute break, during which every lookup is an instant "not now".
    async fn get_json(&self, url: &str, ct: &CancellationToken) -> Answer {
        if self.cool_down_until().is_some_and(|until| Instant::now() < until) {
            return Answer::not_now();
        }

        let mut last_call = tokio::select! {
            guard = self.gate.lock() => guard,
            () = ct.cancelled() => return Answer::not_now(),
        };
        if let Some(last) = *last_call {
            let gap = (last + Self::MINIMUM_GAP).saturating_duration_since(Instant::now());
            if !gap.is_zero() {
                tokio::select! {
                    () = tokio::time::sleep(gap) => {}
                    () = ct.cancelled() => return Answer::not_now(),
                }
            }
        }
        *last_call = Some(Instant::now());

        let response = tokio::select! {
            response = self.client.get(url).send() => response,
            () = ct.cancelled() => return Answer::not_now(),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => return self.failure(&error.to_string()),
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let wait = retry_after_delta(response.headers()).unwrap_or(Self::DEFAULT_COOLDOWN);
            *self.cool_down_until.lock() = Some(Instant::now() + wait);
            info!("KuGou asked Octo to wait {}s", wait.as_secs());
            return Answer::not_now();
        }
        if status == StatusCode::NOT_FOUND {
            return self.succeeded(None);
        }
        if !status.is_success() {
            return self.failure(&format!("HTTP {}", status.as_u16()));
        }

        let body = tokio::select! {
            body = response.bytes() => body,
            () = ct.cancelled() => return Answer::not_now(),
        };
        match body
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string()))
        {
            Ok(json) => self.succeeded(Some(json)),
            Err(error) => self.failure(&error),
        }
    }

    fn succeeded(&self, json: Option<Value>) -> Answer {
        self.failures.store(0, Ordering::SeqCst);
        Answer {
            json,
            transient: false,
        }
    }

    fn failure(&self, why: &str) -> Answer {
        debug!("KuGou request failed: {why}");
        if self.failures.fetch_add(1, Ordering::SeqCst) + 1 >= Self::FAILURES_BEFORE_BREAK {
            self.failures.store(0, Ordering::SeqCst);
            *self.cool_down_until.lock() = Some(Instant::now() + Self::BREAK_LENGTH);
            warn!(
                "KuGou failed {} times in a row; not asking it for {} minutes",
                Self::FAILURES_BEFORE_BREAK,
                Self::BREAK_LENGTH.as_secs() / 60
            );
        }
        Answer::not_now()
    }
}

#[async_trait]
impl ILyricsSource for KugouLyricsSource {
    fn key(&self) -> &str {
        "kugou"
    }

    async fn find(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsLookup {
        let search = self.search(query, ct).await;
        self.find_in(query, &search, ct).await
    }

    async fn search(&self, query: &LyricsQuery, ct: &CancellationToken) -> LyricsSearch {
        // An answer of a shape KuGou never sends threw in C#, which the service caught as a
        // failed lookup.
        self.search_inner(query, ct)
            .await
            .unwrap_or_else(|_| LyricsSearch::failed())
    }

    async fn fetch(&self, id: &str, ct: &CancellationToken) -> LyricsLookup {
        self.fetch_for(id, None, ct).await
    }
}

#[cfg(test)]
#[path = "kugou_lyrics_source_tests.rs"]
pub(crate) mod tests;
