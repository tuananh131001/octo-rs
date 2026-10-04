//! Port of the part of `Services/Common/PathHelper.cs` that lyrics need.
//!
// STUB(2-A tagging): replaced when 2-A lands. Only `FileTitle` exists here, ported whole, because
// `LyricsText.QueryTitle` calls it; 2-A owns PathHelper and brings the rest (layouts, sanitising).

use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::dotnet::{eq_ignore_case, starts_with_ignore_case};

static ANNOTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[]([^\)\]]*)[\)\]]").expect("a fixed pattern compiles"));

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("a fixed pattern compiles"));

/// Words that only ever describe how a video was uploaded, never which recording it is.
/// (A `HashSet` with `StringComparer.OrdinalIgnoreCase`.)
const UPLOAD_NOISE: [&str; 19] = [
    "official",
    "music",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "hd",
    "hq",
    "4k",
    "8k",
    "1080p",
    "720p",
    "480p",
    "mv",
    "m/v",
    "clip",
    "videoclip",
];

/// A title as a file name. Drops a leading "Artist - " and any bracket that is nothing but
/// upload noise ("(Official Video)", "[HD]"), and keeps every other annotation, because
/// "(Live)", "[Remix]" and "(feat. X)" each name a different recording.
///
/// Naming used to strip EVERY bracket, so "Song (Live)" and "Song" landed on one path and
/// the second download deleted the first: the silent collapse of versions a library must
/// never suffer (#53).
pub fn file_title(title: &str, artist: &str) -> String {
    let mut t = title.trim().to_string();
    if t.is_empty() {
        return t;
    }
    let a = artist.trim();
    let prefix = format!("{a} - ");
    if !a.is_empty() && starts_with_ignore_case(&t, &prefix) {
        // OrdinalIgnoreCase compares character for character, so the prefix is as many
        // characters long in the title as in the artist.
        let cut: usize = t.chars().take(prefix.chars().count()).map(char::len_utf8).sum();
        t = t[cut..].trim().to_string();
    }

    let kept = ANNOTATION.replace_all(&t, |caps: &Captures| {
        let inner = caps.get(1).map_or("", |m| m.as_str());
        let words: Vec<&str> = inner
            .split([' ', '-', '_'])
            .filter(|word| !word.is_empty())
            .collect();
        if !words.is_empty()
            && words
                .iter()
                .all(|word| UPLOAD_NOISE.iter().any(|noise| eq_ignore_case(noise, word)))
        {
            String::new()
        } else {
            caps[0].to_string()
        }
    });
    let kept = WHITESPACE.replace_all(&kept, " ").trim().to_string();
    // A title that is only noise ("(Official Video)") keeps its original text rather than
    // becoming an empty file name.
    if kept.is_empty() { t } else { kept }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_title_drops_the_artist_and_upload_noise_but_keeps_versions() {
        assert_eq!(
            file_title("Massive Attack - Teardrop", "Massive Attack"),
            "Teardrop"
        );
        assert_eq!(file_title("Song (Live) [Official Video]", "X"), "Song (Live)");
        assert_eq!(file_title("Song [HD] (feat. Guest)", "X"), "Song (feat. Guest)");
        assert_eq!(file_title("(Official Video)", "X"), "(Official Video)");
        assert_eq!(file_title("  ", "X"), "");
    }
}
