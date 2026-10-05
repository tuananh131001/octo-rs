//! Port of `Services/Lyrics/LyricsfileReader.cs`.

use std::sync::LazyLock;

use regex::Regex;

use super::lyrics_text::{LyricLine, LyricWord};
use crate::common::dotnet::{is_blank, is_word_char, word_boundary_view};

static ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)-\s+(\w+):\s*(.*)$").expect("a fixed pattern"));
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)(\w+):\s*(.*)$").expect("a fixed pattern"));

#[derive(Default)]
struct Entry {
    text: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    words: Vec<Entry>,
}

/// One `- key: value` or `key: value` row, its parts sliced from the original row. Matched
/// over [`word_boundary_view`], so `\w` takes what .NET's did.
struct Row<'a> {
    indent: usize,
    key: &'a str,
    value: &'a str,
}

fn read_row<'a>(pattern: &Regex, raw: &'a str) -> Option<Row<'a>> {
    let view = word_boundary_view(raw);
    let caps = pattern.captures(&view)?;
    let part = |index: usize| caps.get(index).map_or("", |m| &raw[m.start()..m.end()]);
    Some(Row {
        // Whitespace is all in the Basic Multilingual Plane: characters are UTF-16 units here.
        indent: part(1).chars().count(),
        key: part(2),
        value: part(3),
    })
}

/// The timed lines of a Lyricsfile, the YAML document LRCLIB answers with beside its LRC. It
/// is the only place LRCLIB keeps word timing (an entry's hasWordSync), under lines[].words[].
///
/// Deliberately narrow rather than a YAML library for one field: it reads the lines block and
/// its text, start_ms, end_ms and words, and nothing else. Anything it does not expect (a
/// block scalar in a line, a key it cannot place) makes it give up with None, and the caller
/// falls back to the LRC, which is never worse than before.
pub struct LyricsfileReader;

impl LyricsfileReader {
    pub fn read_lines(yaml: Option<&str>) -> Option<Vec<LyricLine>> {
        let yaml = yaml.filter(|yaml| !is_blank(yaml))?;
        let rows: Vec<String> = yaml
            .replace("\r\n", "\n")
            .split('\n')
            .map(str::to_string)
            .collect();
        let entries = parse(&rows)?;
        if entries.is_empty() {
            return None;
        }
        let lines: Vec<LyricLine> = entries
            .iter()
            .filter(|entry| entry.start.is_some() && entry.text.is_some())
            .map(to_line)
            .collect();
        if lines.is_empty() { None } else { Some(lines) }
    }

    /// A one-line scalar: 'single' ('' is a quote), "double" (backslash escapes), or
    /// plain. None for anything else, a block scalar included.
    pub fn scalar(value: &str) -> Option<String> {
        let trimmed = value.trim();
        let Some(first) = trimmed.chars().next() else {
            return Some(String::new());
        };
        if first == '\'' {
            if trimmed.chars().count() < 2 || !trimmed.ends_with('\'') {
                return None;
            }
            return Some(trimmed[1..trimmed.len() - 1].replace("''", "'"));
        }
        if first == '"' {
            if trimmed.chars().count() < 2 || !trimmed.ends_with('"') {
                return None;
            }
            // Regex.Unescape threw on a bad escape, and the reader's catch gave up on the file.
            return regex_unescape(&trimmed[1..trimmed.len() - 1]);
        }
        if first == '|' || first == '>' {
            return None;
        }
        Some(match trimmed.find(" #") {
            Some(comment) => trimmed[..comment].trim_end().to_string(),
            None => trimmed.to_string(),
        })
    }
}

fn parse(rows: &[String]) -> Option<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut in_lines = false;
    let mut line_indent: Option<usize> = None;
    let mut word_indent: Option<usize> = None;
    let mut in_words = false;
    // Whether the current line (the last entry) and its current word (its last word) exist.
    let mut has_line = false;
    let mut has_word = false;

    for raw in rows {
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }
        let indent = raw.chars().count() - raw.trim_start().chars().count();

        if indent == 0 {
            // A top-level key: the lines block starts or ends here.
            in_lines = raw.starts_with("lines:");
            if in_lines {
                let header = raw.trim();
                if header != "lines:" && header != "lines: []" {
                    return None;
                }
            }
            in_words = false;
            continue;
        }
        if !in_lines {
            continue;
        }

        if let Some(item) = read_row(&ITEM, raw) {
            let line_at = *line_indent.get_or_insert(item.indent);
            if item.indent == line_at {
                entries.push(Entry::default());
                has_line = true;
                has_word = false;
                in_words = false;
                let line = entries.last_mut().expect("just added");
                if !set(line, item.key, item.value) {
                    return None;
                }
            } else if in_words && has_line && item.indent > line_at {
                word_indent.get_or_insert(item.indent);
                let line = entries.last_mut().expect("has_line");
                line.words.push(Entry::default());
                has_word = true;
                let word = line.words.last_mut().expect("just added");
                if !set(word, item.key, item.value) {
                    return None;
                }
            } else {
                return None;
            }
            continue;
        }

        let field = read_row(&FIELD, raw)?;
        if !has_line {
            return None;
        }
        // C#: `fieldIndent > lineIndent`, where a null lineIndent compares false.
        let deeper_than_line = line_indent.is_some_and(|line_at| field.indent > line_at);
        if field.key == "words" && deeper_than_line {
            in_words = true;
            has_word = false;
            continue;
        }
        // A word's fields sit deeper than its "- "; anything shallower belongs to the line,
        // and ends its words.
        let line = entries.last_mut().expect("has_line");
        let to_word = in_words && has_word && word_indent.is_some_and(|deeper| field.indent > deeper);
        let target = if to_word {
            line.words.last_mut().expect("has_word")
        } else {
            in_words = false;
            line
        };
        if !set(target, field.key, field.value) {
            return None;
        }
    }
    Some(entries)
}

fn set(entry: &mut Entry, key: &str, value: &str) -> bool {
    match key {
        "text" => match LyricsfileReader::scalar(value) {
            Some(text) => {
                entry.text = Some(text);
                true
            }
            None => false,
        },
        "start_ms" => match value.trim().parse::<i64>() {
            Ok(start) => {
                entry.start = Some(start);
                true
            }
            Err(_) => false,
        },
        "end_ms" => {
            let value = value.trim();
            if matches!(value, "" | "null" | "~") {
                return true;
            }
            match value.parse::<i64>() {
                Ok(end) => {
                    entry.end = Some(end);
                    true
                }
                Err(_) => false,
            }
        }
        // A key this reader does not use (a voice, say) is skipped, never guessed at.
        _ => true,
    }
}

/// A line with its words placed in its text, in order. A word that cannot be
/// found where the one before it ended leaves the line timed as a whole.
fn to_line(entry: &Entry) -> LyricLine {
    let text = entry.text.as_deref().unwrap_or("").trim().to_string();
    let mut words: Vec<LyricWord> = Vec::new();
    let mut cursor = 0;
    for word in &entry.words {
        let piece = word.text.as_deref().unwrap_or("").trim();
        let Some(start) = word.start.filter(|_| !piece.is_empty()) else {
            continue;
        };
        let Some(at) = text[cursor..].find(piece).map(|found| found + cursor) else {
            words.clear();
            break;
        };
        let mut to = at + piece.len();
        while let Some(next) = text[to..].chars().next().filter(|c| c.is_whitespace()) {
            to += next.len_utf8();
        }
        words.push(LyricWord::new(start, word.end, at, to));
        cursor = to;
    }

    // A word's end is the next one's start when the file does not say.
    for index in 0..words.len().saturating_sub(1) {
        if words[index].end_ms.is_none() {
            words[index].end_ms = Some(words[index + 1].start_ms);
        }
    }

    LyricLine {
        start_ms: entry.start.unwrap_or(0),
        text,
        end_ms: words.last().and_then(|word| word.end_ms).or(entry.end),
        words,
    }
}

/// `Regex.Unescape`: the escapes a .NET pattern knows, turned into their characters. None
/// where .NET threw (a dangling backslash, short hex, an unknown letter escape).
fn regex_unescape(text: &str) -> Option<String> {
    if !text.contains('\\') {
        return Some(text.to_string());
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut index = 0;
    let hex = |units: &[u16], from: usize, count: usize| -> Option<u16> {
        let digits = units.get(from..from + count)?;
        let text = String::from_utf16(digits).ok()?;
        if !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(&text, 16).ok()
    };
    while index < units.len() {
        let unit = units[index];
        if unit != u16::from(b'\\') {
            out.push(unit);
            index += 1;
            continue;
        }
        index += 1;
        let escape = *units.get(index)?;
        index += 1;
        let ascii = u8::try_from(escape).ok().map(char::from);
        let value: u16 = match ascii {
            Some('0'..='7') => {
                // Up to three octal digits, the high bits cut off past 0377.
                let mut value: u32 = 0;
                let mut count = 0;
                index -= 1;
                while count < 3 {
                    match units.get(index).and_then(|&u| u8::try_from(u).ok()) {
                        Some(digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            index += 1;
                            count += 1;
                        }
                        _ => break,
                    }
                }
                (value & 0xFF) as u16
            }
            Some('x') => {
                let value = hex(&units, index, 2)?;
                index += 2;
                value
            }
            Some('u') => {
                let value = hex(&units, index, 4)?;
                index += 4;
                value
            }
            Some('a') => 0x07,
            Some('b') => 0x08,
            Some('e') => 0x1B,
            Some('f') => 0x0C,
            Some('n') => 0x0A,
            Some('r') => 0x0D,
            Some('t') => 0x09,
            Some('v') => 0x0B,
            Some('c') => {
                // \cX: a control character, \ca read as \cA.
                let mut control = *units.get(index)?;
                index += 1;
                if (u16::from(b'a')..=u16::from(b'z')).contains(&control) {
                    control -= u16::from(b'a' - b'A');
                }
                let control = control.wrapping_sub(u16::from(b'@'));
                if control >= u16::from(b' ') {
                    return None;
                }
                control
            }
            _ => {
                // An escaped word character that is not an escape is an error; anything else
                // stands for itself.
                let c = char::from_u32(u32::from(escape)).unwrap_or('\u{FFFD}');
                if is_word_char(c) {
                    return None;
                }
                escape
            }
        };
        out.push(value);
    }
    Some(String::from_utf16_lossy(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LyricsWordTimingTests.Lyricsfile_WordsBecomeWordTiming.
    #[test]
    fn lyricsfile_words_become_word_timing() {
        let yaml = "version: '1.0'
metadata:
  title: 'Song'
  artist: 'Someone'
lines:
  - text: 'Hello there, it''s me'
    start_ms: 1000
    end_ms: 3000
    words:
      - text: 'Hello '
        start_ms: 1000
        end_ms: 1400
      - text: \"there, \"
        start_ms: 1400
      - text: 'it''s '
        start_ms: 2000
        end_ms: 2300
      - text: 'me'
        start_ms: 2300
        end_ms: 3000
  - text: 'Second line'
    start_ms: 4000
plain: |
  Hello there, it's me";

        let lines = LyricsfileReader::read_lines(Some(yaml)).expect("lines");

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Hello there, it's me");
        assert_eq!(
            lines[0]
                .words
                .iter()
                .map(|word| word.start_ms)
                .collect::<Vec<_>>(),
            vec![1000, 1400, 2000, 2300]
        );
        assert_eq!(lines[0].words[1].end_ms, Some(2000));
        assert_eq!(
            lines[0]
                .words
                .iter()
                .map(|word| lines[0].word_text(word))
                .collect::<Vec<_>>(),
            vec!["Hello ", "there, ", "it's ", "me"]
        );
        assert!(lines[1].words.is_empty());
    }

    /// LyricsWordTimingTests.Lyricsfile_UnexpectedShape_IsNullSoTheLrcIsUsed.
    #[test]
    fn lyricsfile_unexpected_shape_is_null_so_the_lrc_is_used() {
        assert!(
            LyricsfileReader::read_lines(Some(
                "version: '1.0'\nlines:\n  - text: |\n      block\n    start_ms: 1"
            ))
            .is_none()
        );
        assert!(LyricsfileReader::read_lines(None).is_none());
    }

    /// The Lyricsfile half of LyricsWordTimingTests.Lrclib_HasWordSync_UsesTheLyricsfileWords
    /// (the LRCLIB parsing around it is ported with the LRCLIB source).
    #[test]
    fn lrclibs_lyricsfile_gives_word_timing() {
        let yaml = "version: '1.0'\nlines:\n  - text: 'Hello me'\n    start_ms: 1000\n    words:\n      - text: 'Hello '\n        start_ms: 1000\n      - text: 'me'\n        start_ms: 1500\n        end_ms: 1900\n";
        let lines = LyricsfileReader::read_lines(Some(yaml)).expect("lines");
        assert_eq!(lines[0].words.len(), 2);
        assert_eq!(lines[0].words[0].end_ms, Some(1500));
        assert_eq!(lines[0].end_ms, Some(1900));
    }

    #[test]
    fn scalars_read_as_yaml_writes_them() {
        assert_eq!(LyricsfileReader::scalar("  'it''s'  ").as_deref(), Some("it's"));
        assert_eq!(
            LyricsfileReader::scalar(r#""a\tb\u00e9\x41\\\"c""#).as_deref(),
            Some("a\tbéA\\\"c")
        );
        assert_eq!(
            LyricsfileReader::scalar("plain words # a comment").as_deref(),
            Some("plain words")
        );
        assert_eq!(LyricsfileReader::scalar("").as_deref(), Some(""));
        assert_eq!(LyricsfileReader::scalar("'unclosed"), None);
        assert_eq!(LyricsfileReader::scalar("'"), None);
        assert_eq!(LyricsfileReader::scalar("|"), None);
        assert_eq!(LyricsfileReader::scalar(r#""bad \q escape""#), None);
        assert_eq!(LyricsfileReader::scalar(r#""short \x4""#), None);
        assert_eq!(
            LyricsfileReader::scalar(r#""\uD83C\uDFB5 \101 \cA""#).as_deref(),
            Some("🎵 A \u{1}")
        );
    }

    #[test]
    fn a_word_not_found_where_the_last_ended_leaves_the_line_whole() {
        let yaml = "lines:\n  - text: 'one two'\n    start_ms: 10\n    end_ms: 90\n    words:\n      - text: 'two'\n        start_ms: 10\n      - text: 'one'\n        start_ms: 50\n";
        let lines = LyricsfileReader::read_lines(Some(yaml)).expect("lines");
        assert!(lines[0].words.is_empty());
        assert_eq!(lines[0].end_ms, Some(90));
    }

    #[test]
    fn rows_it_cannot_place_give_up() {
        // A field before any line, a bad number, and a "lines:" with something after it.
        assert!(LyricsfileReader::read_lines(Some("lines:\n  text: 'x'\n")).is_none());
        assert!(LyricsfileReader::read_lines(Some("lines:\n  - text: 'x'\n    start_ms: soon\n")).is_none());
        assert!(LyricsfileReader::read_lines(Some("lines: [1]\n")).is_none());
        assert!(LyricsfileReader::read_lines(Some("lines: []\n")).is_none());
        // Other blocks and comments are passed over.
        let lines = LyricsfileReader::read_lines(Some(
            "# comment\nmeta:\n  - text: 'not a line'\n    start_ms: 1\nlines:\n  # inside\n  - text: x\n    start_ms: 5\n    voice: v1\n",
        ))
        .expect("lines");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "x");
    }
}
