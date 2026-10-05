//! The candidate matching of `Services/Soulseek/SoulseekDownloadService.cs`: the pure statics
//! that decide which peer file answers a request (the searches to run, the filename, length and
//! version checks, the quality ranking). The service itself, and the deny-list seam that needs
//! the rejected-peer store, are `octo::services::soulseek::soulseek_download_service` (task 4-C
//! ports the rest of it there).
//!
//! The regexes run over the views `common::dotnet` provides where .NET's character classes
//! differ from Rust's: [`dotnet::word_boundary_view`] for `\b`, [`dotnet::utf16_class_view`]
//! for `\d`, `\p{L}` and `\p{N}`.

use std::sync::LazyLock;

use regex::Regex;

use crate::common::dotnet::{utf16_class_view, utf16_len, word_boundary_view};
use crate::common::live_version;
use crate::common::song_identity::{SongIdentity, SongQuery};
use crate::soulseek::soulseek_client::{SoulseekFileHit, normalize_extension};

fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a fixed pattern compiles")
}

/// The Soulseek searches for a song, in order, from [`SongIdentity::query_variants`].
///
/// Peers name files, not catalogue entries, so a query carrying a bracket finds nothing:
/// Last.fm and YouTube titles such as "Adele - Hello" or "Long Season [LIVE][4K]" are
/// searched as "Adele Hello" and "Long Season". A title that is only an annotation,
/// Mezzanine's "(Exchange)", keeps it. Then the stylized spelling read as letters
/// ("suicideboys SUICIDE", for a peer who tagged it that way), and the title alone last.
/// At most two queries with the artist and one without: each search waits seconds.
pub fn search_queries(title: &str, artist: &str) -> Vec<SongQuery> {
    let variants = SongIdentity::query_variants(title, artist);
    let with_artist = variants
        .iter()
        .filter(|query| !query.artist.is_empty() && !query.title.contains(['(', '[', '{']))
        .take(2);
    let title_only = variants.iter().filter(|query| query.artist.is_empty()).take(1);
    let queries: Vec<SongQuery> = with_artist.chain(title_only).cloned().collect();
    if queries.is_empty() {
        vec![SongQuery::new(title.trim(), artist.trim())]
    } else {
        queries
    }
}

/// The searches to run, in order, and whether each is read strictly. Strict means the title must
/// appear as a phrase in the filename and the file must state a length within the window. The
/// title-only search needs that because the artist is gone. The album search needs it because it
/// lists the whole record, so every other track on it is an answer too.
pub fn planned_queries(
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration_seconds: Option<i32>,
) -> Vec<(SongQuery, bool)> {
    let mut planned: Vec<(SongQuery, bool)> = search_queries(title, artist)
        .into_iter()
        .map(|q| {
            let strict = q.artist.is_empty();
            (q, strict)
        })
        .collect();
    if let Some(by_album) = album_query(Some(title), Some(artist), album, duration_seconds) {
        planned.push((by_album, true));
    }
    planned
}

/// Album names that say nothing about which record a song is on.
const PLACEHOLDER_ALBUMS: [&str; 7] = [
    "unknown",
    "unknown album",
    "single",
    "singles",
    "non album",
    "non album single",
    "non album tracks",
];

static SINGLE_OR_EP_SUFFIX: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)\s*-\s*(Single|EP)$"));
static BRACKETED: LazyLock<Regex> = LazyLock::new(|| rx(r"\s*[\(\[\{][^\)\]\}]*[\)\]\}]"));

/// The album name without a trailing " - Single" or " - EP".
fn record_name(album: Option<&str>) -> String {
    SINGLE_OR_EP_SUFFIX
        .replace_all(album.unwrap_or("").trim(), "")
        .trim()
        .to_string()
}

fn is_placeholder_album(record: &str) -> bool {
    PLACEHOLDER_ALBUMS.contains(&space_normalize(&SongIdentity::plain(record)).as_str())
}

/// The album's words with every bracket taken off.
fn without_brackets(record: &str) -> String {
    BRACKETED.replace_all(record, "").trim().to_string()
}

/// Artist and album, for a peer who files by folder and names tracks only by number and title, so
/// the artist and title search never reaches the file. None when the album would add nothing: it
/// is empty, a placeholder, the title itself, or the length is unknown. Without a length the
/// strict reading cannot tell which track on the record is the one asked for. Never carries
/// "flac": slskd matches words in paths, and few paths say it.
pub fn album_query(
    title: Option<&str>,
    artist: Option<&str>,
    album: Option<&str>,
    duration_seconds: Option<i32>,
) -> Option<SongQuery> {
    if !duration_seconds.is_some_and(|d| d > 0) {
        return None;
    }
    let title = title.unwrap_or("");
    let who = artist.unwrap_or("").trim();
    let record = record_name(album);
    if who.is_empty() || record.is_empty() {
        return None;
    }
    if is_placeholder_album(&record) {
        return None;
    }
    if SongIdentity::key(&record) == SongIdentity::key(title)
        || SongIdentity::same_title(&record, title, None).is_same()
    {
        return None;
    }
    // A title that is the artist's name, or a phrase of the album's, is in the filenames of the
    // record's other tracks too ("Artist - 03 - Another Song"), so the strict reading would take
    // any of them for the song.
    if SongIdentity::key(title) == SongIdentity::key(who)
        || SongIdentity::same_title(title, who, None).is_same()
    {
        return None;
    }
    if leaf_contains_title_phrase(&SongIdentity::plain(&record), title) {
        return None;
    }
    // A bracket finds nothing on Soulseek, the same reason SearchQueries drops one from a title.
    let words = without_brackets(&record);
    // SongQuery's Title is just the search words here; Text reads "artist album".
    if words.is_empty() {
        None
    } else {
        Some(SongQuery::new(words, who))
    }
}

/// The words of an album search: the artist and the album, with " - Single" or " - EP" and any
/// bracket taken off, as a song's album search does. None for a placeholder album name.
pub fn album_search_text(artist: Option<&str>, album_title: Option<&str>) -> Option<String> {
    let who = artist.unwrap_or("").trim();
    let record = record_name(album_title);
    if who.is_empty() || record.is_empty() {
        return None;
    }
    if is_placeholder_album(&record) {
        return None;
    }
    let words = without_brackets(&record);
    if words.is_empty() {
        None
    } else {
        Some(format!("{who} {words}"))
    }
}

/// Correct rips of the same recording drift by a second or two between masterings.
/// A different recording does not.
///
/// Measured over two full walks of the same album: every correctly-matched track came
/// in within 4s of the catalog length, while wrong ones were 11s, 12s, 93s, 98s and
/// 140s out. 8 sits in that gap. An earlier 15 was too generous and let two dub mixes
/// through at 11s and 12s.
pub const DURATION_TOLERANCE_SECONDS: i32 = 8;

/// Whether a file is a version of the song the request did not ask for: a live take, a
/// remix, a dub, a sped-up upload. This is the only signal that separates "Group Four" from
/// "Group Four (Security Forces dub)", whose runtimes are two seconds apart.
///
/// The same reading TrackMatchComparer applies to the title AcoustID identified, so a peer
/// is never chosen for a file the verification would then reject, delete and deny-list. A
/// remaster, an explicit tag or an "Original Mix" is the same recording and passes.
pub fn adds_version(filename: &str, title: &str) -> bool {
    !SongIdentity::added_versions(title, &leaf_title(filename), None).is_empty()
}

/// How many peers' folders are looked in when a search finds only lossy copies.
pub const PEER_FOLDERS_TO_BROWSE: usize = 3;

/// How long a peer gets to list one folder.
pub const BROWSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The folder a remote file sits in, as the peer names it; empty at the share's root.
pub fn folder_of_file(filename: &str) -> &str {
    match filename.rfind(['\\', '/']) {
        Some(slash) if slash > 0 => &filename[..slash],
        _ => "",
    }
}

/// Whether a plainly named file sits in a live album's folder: "Decade (live at the El
/// Mocambo) (2010)/17 - Smile in Your Sleep.flac" is the live take, though its own name says
/// nothing. The album folder and the one above it are read (a disc folder sits between);
/// not the share's top level. Never when the request itself asks for a live song or album.
pub fn from_live_folder(filename: &str, title: &str, album: Option<&str>) -> bool {
    if live_version::requested(Some(title), album) {
        return false;
    }
    let normalized = filename.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 1 {
        return false;
    }
    let folders = &parts[..parts.len() - 1];
    folders[folders.len().saturating_sub(2)..]
        .iter()
        .any(|part| live_version::mentions(Some(part)))
}

/// The file's name without its extension, as a title.
fn leaf_title(filename: &str) -> String {
    let leaf = leaf_of(filename);
    match leaf.rfind('.') {
        Some(dot) if dot > 0 => leaf[..dot].to_string(),
        _ => leaf.to_string(),
    }
}

/// Extensions where a bigger file means a longer or higher-resolution recording
/// rather than a better one. Used to decide which way the size tiebreak points.
const LOSSLESS_EXTENSIONS: [&str; 7] = ["flac", "wav", "alac", "ape", "aiff", "aif", "wv"];

/// The last signal left when everything above it ties, and it ties often: slskd
/// reports queue length and upload speed per RESPONSE, not per file, so every file
/// one peer offers carries identical values and size is what actually separates them.
///
/// Which direction helps depends on what is being chased. Chasing lossless, the
/// smaller of two otherwise-equal candidates is the CD rip rather than the hi-res
/// transfer, which is the same preference QualityPenalty encodes and the only way to
/// express it when a peer reports no bit depth at all. Chasing a lossy format, the
/// bigger file is simply the higher bitrate, and preferring the smaller one would
/// walk an mp3 library down to its worst copy of every track.
pub fn size_sort_key(size: i64, preferred_extension: Option<&str>) -> i64 {
    if LOSSLESS_EXTENSIONS.contains(&normalize_extension(preferred_extension, "").as_str()) {
        size
    } else {
        -size
    }
}

/// How far a candidate sits from ordinary CD quality, 16-bit/44.1kHz.
///
/// CD is the target because it is what the master almost always was: a 24/96 transfer
/// of a 1998 pop record carries no more music than the 16/44.1 one, at several times
/// the bytes on a disk the user is paying for and a transfer that takes proportionally
/// longer over Soulseek. Hi-res is ranked down rather than rejected, because sometimes
/// it is the only copy a peer has.
///
/// Unknown sits deliberately between the two. Most peers report neither field, so
/// treating unknown as hi-res would bury the majority of a normal search, and treating
/// it as CD would let an unlabelled 24/96 outrank a labelled 16/44.1.
pub fn quality_penalty(h: &SoulseekFileHit) -> i32 {
    let bit_depth_penalty = match h.bit_depth {
        Some(16) => 0,
        None => 3,
        Some(24) => 10,
        Some(d) if d > 24 => 20,
        Some(_) => 5,
    };

    let sample_rate_penalty = match h.sample_rate {
        Some(44100) => 0,
        Some(48000) => 1,
        None => 3,
        Some(88200) => 10,
        Some(96000) => 11,
        Some(176400) => 20,
        Some(192000) => 21,
        Some(r) if r > 96000 => 20,
        Some(r) if r > 48000 => 10,
        Some(_) => 4,
    };

    bit_depth_penalty + sample_rate_penalty
}

/// The last segment of a remote path, either separator.
fn leaf_of(path: &str) -> &str {
    path.split(['\\', '/']).rfind(|s| !s.is_empty()).unwrap_or(path)
}

fn title_tokens(title: &str) -> Vec<String> {
    SongIdentity::plain(title)
        .split([' ', '-', '(', ')', '[', ']', '_', '.', ',', '\'', '"'])
        .filter(|t| !t.is_empty())
        .filter(|t| utf16_len(t) >= 3)
        .map(str::to_string)
        .collect()
}

/// A roman numeral on the end of a title, which TitleTokens cannot see.
///
/// Tokens shorter than 3 characters are dropped so that "DNA." and "M.I.A." are not
/// over-filtered, and that quietly deletes the entire difference between "Trilogy I"
/// and "Trilogy II": both reduce to the single token "trilogy", so either file
/// satisfies a request for the other. Anchored to the end so an "I" inside a sentence
/// is left alone, and matched on word boundaries in the filename so "I" does not find
/// itself inside "II" and "V" does not find itself inside "IV".
static TRAILING_ROMAN_NUMERAL: LazyLock<Regex> =
    LazyLock::new(|| rx(r"(?i)\b(I|II|III|IV|V|VI|VII|VIII|IX|X)\b\s*$"));

/// Does the FILENAME look like the track we asked for?
///
/// Two things here were wrong and both let the wrong song through. It matched the
/// whole path, so a folder named "Mezzanine Remix Tapes '98" satisfied a search for
/// the track "Mezzanine" while the file inside was a different song entirely. And it
/// accepted any single token, so "Group Four" would have been satisfied by "Four
/// Seasons". Now every significant token must appear in the leaf name. Tokens are
/// three or more chars so short titles like "DNA." or "M.I.A." are not over-filtered, with a
/// trailing roman numeral handled separately since that rule would erase it.
///
/// require_phrase is the title-only fallback's stricter contract. Scattered tokens
/// are enough when the artist was in the query, but with the artist gone they are
/// the whole defense, and "The Truth" scattered across "The Greataxe of Shining
/// Truth" is how a 136 MB dungeon-synth track answered a country-song star. The
/// title must then appear as a contiguous phrase in the leaf, with a leading
/// article allowed to drop ("Truth.flac" still answers "The Truth") and dotted
/// acronyms allowed their compact form ("MIA.flac" still answers "M.I.A.").
pub fn filename_plausibly_matches_title(filename: &str, title: &str, require_phrase: bool) -> bool {
    if filename.is_empty() || title.is_empty() {
        return true;
    }
    // Folded the way titles are, so "Huntin’ Wabbitz" finds "Huntin' Wabbitz" and
    // "Hoppípolla" finds "Hoppipolla".
    let leaf = SongIdentity::plain(leaf_of(filename));

    let trimmed = title.trim();
    if let Some(wanted_roman) = TRAILING_ROMAN_NUMERAL.captures(&word_boundary_view(trimmed)) {
        let numeral = &trimmed[wanted_roman.get(1).expect("group 1 matched").range()];
        let in_leaf = rx(&format!(r"(?i)\b{}\b", regex::escape(numeral)));
        if !in_leaf.is_match(&word_boundary_view(&leaf)) {
            return false;
        }
    }

    // Phrase evidence supersedes token scattering rather than adding to it: the
    // token rule would demand a "the" from a filename that legitimately dropped
    // the article, and its scattered matches are exactly what this mode distrusts.
    if require_phrase {
        return leaf_contains_title_phrase(&leaf, title)
            || leaf_contains_title_phrase(&stylized(&leaf), &SongIdentity::fold_stylized(title));
    }

    // A stylized title ("$UICIDE") and a peer who spelled it out ("Suicide"), either way
    // round: each word may match as written or with its stylized characters read as letters.
    let tokens = title_tokens(title);
    // Every word of the title is under three letters ("Up", "M.I.A.", "I Am"), so the token rule
    // has nothing to check, and this used to let any file through. Ask for the whole title instead,
    // as words of their own in the filename, spaced or with the dots dropped.
    if tokens.is_empty() {
        return leaf_contains_title_phrase(&leaf, title)
            || leaf_contains_title_phrase(&stylized(&leaf), &SongIdentity::fold_stylized(title));
    }
    let loose_leaf = stylized(&leaf);
    tokens
        .iter()
        .all(|t| leaf.contains(t.as_str()) || loose_leaf.contains(stylized(t).as_str()))
}

fn stylized(value: &str) -> String {
    SongIdentity::plain(&SongIdentity::fold_stylized(value))
}

const LEADING_ARTICLES: [&str; 3] = ["the ", "a ", "an "];

fn leaf_contains_title_phrase(leaf: &str, title: &str) -> bool {
    let leaf_norm = format!(" {} ", space_normalize(leaf));
    let phrase = space_normalize(&SongIdentity::plain(title));
    if phrase.is_empty() {
        return true;
    }

    if leaf_norm.contains(&format!(" {phrase} ")) {
        return true;
    }

    let compact = phrase.replace(' ', "");
    if compact != phrase && utf16_len(&compact) >= 3 && leaf_norm.contains(&format!(" {compact} ")) {
        return true;
    }

    // A dropped leading article is tolerated only when what remains names a whole
    // separator-delimited SEGMENT of the filename. As a bare word it proves
    // nothing: "Truth" is the track in "Jason Aldean - Truth" but a fragment in
    // "The Greataxe of Shining Truth", and word-level tolerance here is precisely
    // the hole the phrase rule exists to close.
    for article in LEADING_ARTICLES {
        let Some(rest) = phrase.strip_prefix(article) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        if leaf_segments(leaf).iter().any(|segment| segment == rest) {
            return true;
        }
    }
    false
}

fn leaf_segments(leaf: &str) -> Vec<String> {
    let without_extension = match leaf.rfind('.') {
        Some(dot) if dot > 0 => &leaf[..dot],
        _ => leaf,
    };
    without_extension
        .split(['-', '_'])
        .filter(|s| !s.is_empty())
        .map(space_normalize)
        .filter(|segment| !segment.is_empty())
        .collect()
}

static NOT_LETTER_OR_NUMBER: LazyLock<Regex> = LazyLock::new(|| rx(r"[^\p{L}\p{N}]+"));
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| rx(r"\s+"));

/// Every run of characters that are not letters or numbers as one space. Run over the UTF-16
/// view: .NET never counted a supplementary-plane character as either, and such a character
/// always falls inside a replaced run, so the view's result is the text's.
fn space_normalize(value: &str) -> String {
    let spaced = NOT_LETTER_OR_NUMBER
        .replace_all(&utf16_class_view(value), " ")
        .into_owned();
    WHITESPACE.replace_all(&spaced, " ").trim().to_string()
}

/// Reject a candidate whose advertised length is nowhere near the known one.
pub fn duration_plausible(
    candidate_seconds: Option<i32>,
    expected_seconds: Option<i32>,
    require_known_length: bool,
) -> bool {
    // Unknown either side is normally not evidence of a bad match, so it passes and
    // the post-download check has the final say. The title-only fallback revokes
    // that benefit of the doubt: when the catalog length is known and the artist is
    // no longer in the query, a candidate that will not say how long it is has
    // already used up its plausibility, and the post-download check rejecting it
    // later still costs the full transfer.
    let Some(c) = candidate_seconds.filter(|&c| c > 0) else {
        return !(require_known_length && expected_seconds.is_some_and(|e| e > 0));
    };
    let Some(e) = expected_seconds.filter(|&e| e > 0) else {
        return true;
    };
    (c - e).abs() <= DURATION_TOLERANCE_SECONDS
}

static BRACKET_GROUP: LazyLock<Regex> = LazyLock::new(|| rx(r"[\(\[]([^\)\]]*)[\)\]]"));
static YEAR: LazyLock<Regex> = LazyLock::new(|| rx(r"^(19|20)\d{2}$"));

/// How many "this is a different recording" signals the candidate carries that the
/// requested title never asked for: the version markers [`SongIdentity`] reads,
/// and every bracketed addition.
///
/// The generic half matters more than the word list. "Angel (Angel Dust)" and
/// "Inertia Creeps (Floating on Dubwise)" are both dub mixes, and neither contains a
/// keyword any sane list would hold, but both are bracketed additions the title did
/// not ask for, and that is the thing they have in common with every other wrong take.
///
/// Ranked rather than rejected, for the generic half: a bracket may only be a peer's own
/// label. A named version the request lacks is rejected before ranking, by AddsVersion.
pub fn variant_penalty(filename: &str, title: &str) -> i32 {
    let leaf = leaf_of(filename);
    let wanted = SongIdentity::plain(title);

    let mut penalty = SongIdentity::added_versions(title, &leaf_title(filename), None).len() as i32;

    for group in BRACKET_GROUP.captures_iter(leaf) {
        let inner_text = SongIdentity::plain(&group[1]);
        let inner = inner_text.trim();
        if inner.is_empty() {
            continue;
        }
        // A year or a format tag is how peers label a good rip, not a different take.
        if YEAR.is_match(&utf16_class_view(inner)) {
            continue;
        }
        if matches!(inner, "flac" | "hi-res" | "hires" | "16-44" | "24-96" | "24-44") {
            continue;
        }
        if !wanted.contains(inner) {
            penalty += 1;
        }
    }

    penalty
}

#[cfg(test)]
#[path = "soulseek_download_service_tests.rs"]
mod tests;
