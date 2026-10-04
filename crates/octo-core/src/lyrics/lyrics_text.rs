//! Port of `Services/Lyrics/LyricsText.cs`: LRC and enhanced LRC read and written, and the
//! credit lines some sources put over a lyric taken out.
//!
//! Regex notes. The C# time tags used `\d`, which in .NET also matches other scripts' digits,
//! after which `long.Parse` threw; the port reads ASCII digits only (known-diffs.md). The
//! credit label counted its `{1,16}` in UTF-16 units, so it is matched over a view of the text
//! in which every supplementary-plane character is two private-use characters. The
//! case-insensitive patterns keep .NET's invariant equivalences: Rust's `(?i)s` would also
//! take `ſ`, so the letter `s` is matched as `[sS]` with case-insensitivity off.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::lyrics_models::LyricsResult;
use crate::common::dotnet::{is_blank, word_boundary_view};
use crate::common::path_helper;

/// One timed word, as a byte range `[from, to)` in its line's text. The range takes the
/// space after the word too, except at the end of the line, which is how enhanced LRC and the
/// OpenSubsonic cues both place a word. `end_ms` is None when nothing says when the word ends.
///
/// (C# held UTF-16 indices; see the module notes of [`crate::lyrics`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LyricWord {
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub from: usize,
    pub to: usize,
}

impl LyricWord {
    pub fn new(start_ms: i64, end_ms: Option<i64>, from: usize, to: usize) -> Self {
        Self {
            start_ms,
            end_ms,
            from,
            to,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LyricLine {
    pub start_ms: i64,
    pub text: String,

    /// Empty when the line is timed as a whole.
    pub words: Vec<LyricWord>,

    /// When the last word ends, when anything says so.
    pub end_ms: Option<i64>,
}

impl LyricLine {
    /// A line timed as a whole.
    pub fn new(start_ms: i64, text: impl Into<String>) -> Self {
        Self {
            start_ms,
            text: text.into(),
            words: Vec::new(),
            end_ms: None,
        }
    }

    /// The text of one of this line's words; empty when the range does not fit the text.
    pub fn word_text(&self, word: &LyricWord) -> &str {
        slice(&self.text, word.from, word.to)
    }
}

fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a fixed pattern compiles")
}

/// A `RegexOptions.IgnoreCase` alternation of plain lower-case words, with .NET's invariant
/// equivalences: every `s` is matched as `[sS]` alone (Rust's case folding would add `ſ`).
fn ignore_case_words(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| word.replace('s', "(?-i:[sS])"))
        .collect::<Vec<_>>()
        .join("|")
}

static TIME_TAG: LazyLock<Regex> =
    LazyLock::new(|| rx(r"\[([0-9]{1,3}):([0-9]{1,2})(?:[.:]([0-9]{1,3}))?\]"));

/// A word's time inside a line, `<01:23.45>`, as enhanced LRC writes it.
static WORD_TAG: LazyLock<Regex> = LazyLock::new(|| rx(r"<([0-9]{1,3}):([0-9]{1,2})(?:[.:]([0-9]{1,3}))?>"));

/// "label: value", where the label is at most three short words. Matched over
/// [`utf16_units_view`], so the lengths count UTF-16 units as .NET did.
static CREDIT_LABEL: LazyLock<Regex> =
    LazyLock::new(|| rx(r"^\s*([^\s:：]{1,16}(?:\s[^\s:：]{1,16}){0,2})\s*[:：]"));

static FEATURE_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| rx(r"(?i)\s*[\(\[]\s*(feat\.?|ft\.?|featuring|with)\s[^\)\]]*[\)\]]"));

/// Contributor roles NetEase and KuGou credit at the top of a lyric. Chinese has no
/// word boundaries, so these match anywhere in the label.
const CJK_CREDITS: [&str; 31] = [
    "作词", "作詞", "作曲", "编曲", "編曲", "制作", "製作", "监制", "監製", "出品", "混音", "母带", "母帶",
    "录音", "錄音", "和声", "和聲", "吉他", "贝斯", "貝斯", "鼓", "弦乐", "弦樂", "版权", "版權", "统筹",
    "統籌", "企划", "企劃", "发行", "發行",
];

/// The rights notices KuGou puts over a lyric ("TME owns the copyright of this
/// translation", "not to be covered without permission"). No colon, so matched by phrase,
/// and only near the start, where a notice sits and a lyric line saying so would not.
const CJK_NOTICES: [&str; 7] = [
    "著作权",
    "著作權",
    "未经",
    "未經",
    "不得翻唱",
    "版权所有",
    "版權所有",
];

/// The Latin ones match only as whole words, so "Stop: ..." is never taken for "OP".
/// Matched over [`word_boundary_view`], for .NET's `\b`.
static LATIN_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    let words = ignore_case_words(&[
        "op",
        "sp",
        "lyrics",
        "lyricist",
        "composers?",
        "composed",
        "arrangers?",
        "arranged",
        "producers?",
        "produced",
        "writers?",
        "written",
        "mixed",
        "mastered",
        "recorded",
        "vocals",
        "engineers?",
        "engineered",
        "publishers?",
        "published",
        "samples?",
    ]);
    rx(&format!(r"(?i)\b({words})\b"))
});

/// Labels that are credits only near the top: "Artist: X" and "Track: Y" open a
/// lyric some sites keep, and a sung line may start with either word later on.
static EARLY_LATIN_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    let words = ignore_case_words(&["artist", "album", "track", "title"]);
    rx(&format!(r"(?i)^\s*({words})\s*$"))
});

static SECTION_HEADING: LazyLock<Regex> = LazyLock::new(|| rx(r"^\[[^\]]*\]$"));

/// A silence between two words at least this long (ms) is written down.
const WORD_GAP_MS: i64 = 30;

/// `text[from..to]`, or empty when the range does not fit (C# would have thrown; every range
/// the port makes fits).
fn slice(text: &str, from: usize, to: usize) -> &str {
    text.get(from..to).unwrap_or("")
}

/// The text with every supplementary-plane character written as two private-use characters,
/// so a Rust pattern counts what a .NET pattern counted: UTF-16 units. A private-use character
/// is what a lone surrogate was to the patterns here: not a space, not a colon, not a word
/// character, and above U+2E80.
fn utf16_units_view(text: &str) -> Cow<'_, str> {
    if text.chars().all(|c| (c as u32) < 0x10000) {
        return Cow::Borrowed(text);
    }
    let mut view = String::with_capacity(text.len());
    for c in text.chars() {
        if (c as u32) < 0x10000 {
            view.push(c);
        } else {
            view.push_str("\u{E000}\u{E000}");
        }
    }
    Cow::Owned(view)
}

/// `Regex.Replace(text, "")` for the tag patterns.
fn strip(pattern: &Regex, text: &str) -> String {
    pattern.replace_all(text, "").into_owned()
}

/// `raw.Replace("\r\n", "\n").Split('\n')`.
fn lrc_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(str::to_string)
        .collect()
}

fn millis(tag: &Captures) -> i64 {
    let number = |index: usize| -> i64 {
        tag.get(index)
            .and_then(|group| group.as_str().parse().ok())
            .unwrap_or(0)
    };
    let ms = number(1) * 60_000 + number(2) * 1000;
    let Some(fraction) = tag.get(3).map(|group| group.as_str()) else {
        return ms;
    };
    let value: i64 = fraction.parse().unwrap_or(0);
    ms + match fraction.len() {
        1 => value * 100,
        2 => value * 10,
        _ => fraction[..3].parse().unwrap_or(0),
    }
}

/// LRC text: reading, writing and cleaning. A static class in C#.
pub struct LyricsText;

impl LyricsText {
    /// Whether a "label: value" line's label names a contributor role.
    pub fn is_credit_label(name: &str) -> bool {
        CJK_CREDITS.iter().any(|word| name.contains(word)) || LATIN_CREDIT.is_match(&word_boundary_view(name))
    }

    pub fn has_timestamps(text: Option<&str>) -> bool {
        text.is_some_and(|text| TIME_TAG.is_match(text))
    }

    /// Whether a lyric times its words, not only its lines.
    pub fn has_word_tags(text: Option<&str>) -> bool {
        text.is_some_and(|text| WORD_TAG.is_match(text))
    }

    /// The title a lyrics service indexes: no featured-artist credit and no upload
    /// noise, but a "(Live)" or "(Remix)" kept, because those have their own lyrics.
    pub fn query_title(title: &str, artist: &str) -> String {
        FEATURE_SEGMENT
            .replace_all(&path_helper::file_title(title, artist), "")
            .trim()
            .to_string()
    }

    /// Timed lines, sorted, with metadata tags such as [ar:] and [offset:] skipped. A line with
    /// `<mm:ss.xx>` word tags gets its words: each tag starts the text up to the next one, a
    /// tag with nothing after it ends the word before it, and the tags never reach the text.
    /// A line sung at several times carries its words to each, moved by the same amount.
    pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
        let mut lines = Vec::new();
        for raw in lrc_lines(lrc) {
            let tags: Vec<Captures> = TIME_TAG.captures_iter(&raw).collect();
            let Some(first_tag) = tags.first() else {
                continue;
            };
            let first = millis(first_tag);
            let line = parse_words(first, &strip(&TIME_TAG, &raw));
            for tag in &tags {
                let shift = millis(tag) - first;
                lines.push(if shift == 0 {
                    line.clone()
                } else {
                    shift_line(&line, shift)
                });
            }
        }
        // OrderBy is stable, as sort_by_key is.
        lines.sort_by_key(|line| line.start_ms);
        lines
    }

    /// Lines as LRC: [mm:ss.xx] before each, and when a line has words, `<mm:ss.xx>` before
    /// each word and one after the last when its end is known. The standard line tags stay, so
    /// a player that knows nothing of word timing still shows every line at its time.
    pub fn write_lrc<'a>(lines: impl IntoIterator<Item = &'a LyricLine>) -> String {
        let mut lrc = String::new();
        for line in lines {
            lrc.push('[');
            lrc.push_str(&stamp(line.start_ms));
            lrc.push(']');
            let Some(first) = line.words.first() else {
                lrc.push_str(&line.text);
                lrc.push('\n');
                continue;
            };

            lrc.push_str(slice(&line.text, 0, first.from));
            for (index, word) in line.words.iter().enumerate() {
                let next = line.words.get(index + 1);
                let to = next.map_or(line.text.len(), |next| next.from);
                lrc.push('<');
                lrc.push_str(&stamp(word.start_ms));
                lrc.push('>');
                lrc.push_str(slice(&line.text, word.from, word.from.max(to)));
                // A word followed by a pause gets its own end, or a reader stretches it to the
                // next word and lights it late. Readers that only take the line's last end skip
                // this tag harmlessly, since no text follows it.
                if let (Some(next), Some(word_end)) = (next, word.end_ms)
                    && next.start_ms - word_end >= WORD_GAP_MS
                {
                    lrc.push('<');
                    lrc.push_str(&stamp(word_end));
                    lrc.push('>');
                }
            }
            if let Some(end) = line.words.last().and_then(|word| word.end_ms) {
                lrc.push('<');
                lrc.push_str(&stamp(end));
                lrc.push('>');
            }
            lrc.push('\n');
        }
        lrc.trim_end_matches('\n').to_string()
    }

    /// A line's text with every word tag taken out, for a client that asked for lines.
    pub fn strip_word_tags(text: &str) -> String {
        strip(&WORD_TAG, text)
    }

    /// The words as untimed text, one line each, for the legacy getLyrics call.
    pub fn plain_text(result: &LyricsResult) -> String {
        if result.has_synced() {
            Self::parse_lrc(result.synced.as_deref().unwrap_or(""))
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            result
                .plain
                .as_deref()
                .unwrap_or("")
                .replace("\r\n", "\n")
                .trim()
                .to_string()
        }
    }

    /// A lyric's first lines with words in them, for a choice list. (C#'s default count is 2.)
    pub fn preview(result: &LyricsResult, count: usize) -> Vec<String> {
        let lines: Vec<String> = if result.has_synced() {
            Self::parse_lrc(result.synced.as_deref().unwrap_or(""))
                .into_iter()
                .map(|line| line.text)
                .collect()
        } else {
            lrc_lines(result.plain.as_deref().unwrap_or(""))
                .iter()
                .map(|line| line.trim().to_string())
                .collect()
        };
        lines
            .into_iter()
            .filter(|line| !line.is_empty())
            .take(count)
            .collect()
    }

    /// NetEase and KuGou open a lyric with contributor credits, "[00:00.000] 作词 : 周杰伦" and a
    /// dozen more, sometimes as JSON lines, sometimes a mid-song "出品：...". Written through
    /// unfiltered, every such result starts with text that is not the song (#52). A line goes
    /// when it is JSON, when it is a rights notice in the first half minute, or when it reads
    /// "label: value" and either its label is a known credit word, or it sits in the first
    /// fifteen seconds under a label that is not Latin text. An English lyric that happens to
    /// contain a colon keeps its place.
    pub fn strip_credits(lrc: &str) -> String {
        let mut kept: Vec<String> = Vec::new();
        for raw in lrc_lines(lrc) {
            let text = Self::strip_word_tags(&strip(&TIME_TAG, &raw)).trim().to_string();
            if text.starts_with("{\"t\":") {
                continue;
            }

            let at = TIME_TAG.captures(&raw).map_or(i64::MAX, |first| millis(&first));
            if at < 30_000 && CJK_NOTICES.iter().any(|notice| text.contains(notice)) {
                continue;
            }

            // A section heading, "[Intro: Drake]", is never sung.
            if SECTION_HEADING.is_match(&text) {
                continue;
            }

            let view = utf16_units_view(&text);
            if let Some(label) = CREDIT_LABEL.captures(&view) {
                let name = label.get(1).map_or("", |group| group.as_str());
                let non_latin = name.chars().any(|ch| ch as u32 > 0x2E80);
                if Self::is_credit_label(name)
                    || (at < 15_000 && non_latin)
                    || (at < 30_000 && EARLY_LATIN_CREDIT.is_match(name))
                {
                    continue;
                }
            }
            kept.push(raw);
        }
        kept.join("\n").trim().to_string()
    }

    /// Kept under its first name for the NetEase source and its tests.
    pub fn strip_netease_credits(lrc: &str) -> String {
        Self::strip_credits(lrc)
    }
}

fn parse_words(start: i64, body: &str) -> LyricLine {
    let stamps: Vec<Captures> = WORD_TAG.captures_iter(body).collect();
    let Some(first_stamp) = stamps.first() else {
        return LyricLine::new(start, body.trim());
    };

    // Each piece is the text after a stamp, up to the next one. Text before the first stamp
    // starts with the line.
    let mut pieces: Vec<(i64, &str)> = Vec::new();
    let lead = &body[..first_stamp.get(0).map_or(0, |m| m.start())];
    if !is_blank(lead) {
        pieces.push((start, lead));
    }
    for (index, stamp) in stamps.iter().enumerate() {
        let from = stamp.get(0).map_or(0, |m| m.end());
        let to = stamps
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map_or(body.len(), |m| m.start());
        pieces.push((millis(stamp), &body[from..to]));
    }

    let mut text = String::new();
    let mut words: Vec<LyricWord> = Vec::new();
    for (index, (at, piece)) in pieces.iter().enumerate() {
        if piece.is_empty() {
            continue;
        }
        let from = text.len();
        text.push_str(piece);
        if is_blank(piece) {
            continue;
        }
        let end = pieces.get(index + 1).map(|(next_at, _)| *next_at);
        words.push(LyricWord::new(*at, end, from, text.len()));
    }

    // Spaces around the line go, and the words move with the text.
    let cut = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    let placed: Vec<LyricWord> = words
        .into_iter()
        .map(|word| LyricWord {
            from: word.from.saturating_sub(cut).min(trimmed.len()),
            to: word.to.saturating_sub(cut).min(trimmed.len()),
            ..word
        })
        .filter(|word| word.to > word.from)
        .collect();
    LyricLine {
        start_ms: start,
        text: trimmed.to_string(),
        end_ms: placed.last().and_then(|word| word.end_ms),
        words: placed,
    }
}

fn shift_line(line: &LyricLine, by: i64) -> LyricLine {
    LyricLine {
        start_ms: line.start_ms + by,
        text: line.text.clone(),
        end_ms: line.end_ms.map(|end| end + by),
        words: line
            .words
            .iter()
            .map(|word| LyricWord {
                start_ms: word.start_ms + by,
                end_ms: word.end_ms.map(|end| end + by),
                ..*word
            })
            .collect(),
    }
}

/// mm:ss.xx, in hundredths as most players write it; minutes run past 99.
fn stamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!("{:02}:{:02}.{:02}", ms / 60_000, ms / 1000 % 60, ms % 1000 / 10)
}

#[cfg(test)]
#[path = "lyrics_text_tests.rs"]
mod tests;
