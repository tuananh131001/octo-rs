//! The design of generated list covers as numbers, read from Design/cover-design.json, and the
//! library of painted backgrounds from Design/Backgrounds/backgrounds.json. The Octo apps draw
//! their playlist covers from the same files, so the server's covers match theirs: sizes and
//! places are shares of the cover's side.

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::{Context, bail};
use indexmap::IndexMap;
use serde::Deserialize;

use super::design;
use super::text;

#[derive(Clone, Debug, Deserialize)]
pub struct FontFiles {
    pub family: String,
    pub title: String,
    pub line: String,
    pub footer: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TitleNumbers {
    pub top: f32,
    pub weight: i32,
    pub size: f32,
    pub one_line_down_to: f32,
    pub wrap_size: f32,
    pub min_size: f32,
    pub min_px: f32,
    pub max_lines: i32,
    pub line_height: f32,
    pub tracking: f32,
    #[serde(default)]
    pub one_line_min_px: f32,
    #[serde(default)]
    pub wrap_min_px: f32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LineNumbers {
    pub weight: i32,
    pub share_of_title: f32,
    pub min_px: f32,
    pub line_height: f32,
    pub tracking: f32,
    pub show_from_px: i32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FooterNumbers {
    pub weight: i32,
    pub size: f32,
    pub min_px: f32,
    pub bottom: f32,
    pub opacity: f32,
    pub line_height: f32,
    pub tracking: f32,
    pub show_from_px: i32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LayoutNumbers {
    pub margin: f32,
    pub contrast: f64,
    pub tiny_below_px: i32,
    pub title: TitleNumbers,
    pub line: LineNumbers,
    pub footer: FooterNumbers,
}

#[derive(Clone, Debug, Deserialize)]
pub struct VeilTitle {
    pub pad: f64,
    pub falloff: Vec<f64>,
    pub aim_contrast: f64,
    pub min_contrast: f64,
    pub max_drop: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct VeilFooter {
    pub pad: f64,
    pub falloff: Vec<f64>,
    pub contrast: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct VeilYellow {
    pub hues: Vec<f64>,
    pub split: f64,
    pub towards: Vec<f64>,
    pub turn_per_drop: f64,
    pub chroma_from: f64,
    pub chroma_lift: f64,
}

/// How a list's background is picked from the library by its music's colour.
#[derive(Clone, Debug, Deserialize)]
pub struct BackgroundRule {
    pub nearest: i32,
    pub low_chroma_as_grey: f64,
    pub hue_step: f64,
    pub grey_below: f64,
    pub grey_penalty: f64,
    pub chroma_weight: f64,
    pub lightness_weight: f64,
    pub orientation: OrientationRule,
}

/// How a list turns its background: v = (coverPick(id) >>> shift) mod count.
#[derive(Clone, Debug, Deserialize)]
pub struct OrientationRule {
    pub shift: i32,
    pub count: i32,
}

/// How the background is darkened under the words, keeping its colour.
#[derive(Clone, Debug, Deserialize)]
pub struct VeilNumbers {
    pub margin: f64,
    pub refine: i32,
    pub title: VeilTitle,
    pub footer: VeilFooter,
    pub yellow: VeilYellow,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Hue {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

/// One painted background: its file and its colours, strongest first.
#[derive(Clone, Debug, Deserialize)]
pub struct Background {
    pub file: String,
    pub name: String,
    pub family: String,
    #[serde(default)]
    pub hues: Option<Vec<Hue>>,
    pub mean_lightness: f64,
}

impl Background {
    /// Its colours, strongest first (none when the file named none).
    pub fn hues(&self) -> &[Hue] {
        self.hues.as_deref().unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct Document {
    version: i32,
    fonts: FontFiles,
    layout: LayoutNumbers,
    background: BackgroundRule,
    veil: VeilNumbers,
}

#[derive(Deserialize)]
struct Library {
    size: i32,
    backgrounds: Vec<Background>,
}

#[derive(Deserialize)]
struct HueDocument {
    #[serde(default)]
    chroma: Option<f64>,
    #[serde(default)]
    hues: Option<IndexMap<String, f64>>,
}

/// The design of generated list covers, and the library of painted backgrounds.
#[derive(Debug)]
pub struct CoverBook {
    pub version: i32,
    pub fonts: FontFiles,
    pub layout: LayoutNumbers,
    pub veil: VeilNumbers,
    pub background_choice: BackgroundRule,
    pub backgrounds: Vec<Background>,
    /// The side the backgrounds are stored at.
    pub background_size: i32,
    list_chroma: f64,
    exact: HashMap<String, f64>,
    parts: HashMap<String, f64>,
}

static SHIPPED: LazyLock<CoverBook> = LazyLock::new(|| {
    CoverBook::parse(
        design::COVER_DESIGN_JSON,
        design::BACKGROUNDS_JSON,
        Some(design::LIST_HUES_JSON),
    )
    .expect("the cover design shipped inside the app parses")
});

impl CoverBook {
    /// The book shipped inside the app (C# `CoverBook.Default`).
    pub fn shipped() -> &'static CoverBook {
        &SHIPPED
    }

    /// Reads a design, a background library and (optionally) the genre and decade hues. Field
    /// names are matched ignoring case and numbers may be written as strings, as the C#
    /// reader's options allowed.
    pub fn parse(design: &str, library: &str, list_hues: Option<&str>) -> anyhow::Result<CoverBook> {
        let document: Document =
            octo_core::json::web::from_slice(design.as_bytes()).context("the cover design does not parse")?;
        let library: Library = octo_core::json::web::from_slice(library.as_bytes())
            .context("the cover library does not parse")?;
        let hues: Option<HueDocument> = match list_hues {
            Some(text) => Some(
                octo_core::json::web::from_slice(text.as_bytes()).context("the list hues do not parse")?,
            ),
            None => None,
        };
        let backgrounds: Vec<Background> = library
            .backgrounds
            .into_iter()
            .filter(|b| !b.hues().is_empty())
            .collect();
        if backgrounds.is_empty() {
            bail!("the cover library has no backgrounds");
        }
        let mut exact = HashMap::new();
        let mut parts = HashMap::new();
        let list_chroma = hues.as_ref().and_then(|h| h.chroma).unwrap_or(0.14);
        for (name, hue) in hues.iter().flat_map(|h| h.hues.iter().flatten()) {
            exact.entry(song_identity_key(name)).or_insert(*hue);
            // "R&B & Soul" answers to "R&B" and to "Soul" as well.
            for part in name.split(" & ").map(str::trim).filter(|p| !p.is_empty()) {
                parts.entry(song_identity_key(part)).or_insert(*hue);
            }
        }
        Ok(CoverBook {
            version: document.version,
            fonts: document.fonts,
            layout: document.layout,
            veil: document.veil,
            background_choice: document.background,
            backgrounds,
            background_size: library.size,
            list_chroma,
            exact,
            parts,
        })
    }

    /// A stand-in for the music's colour when a list's songs give none: its genre's or decade's
    /// hue. A trailing " Mix" or " Radio" is not part of the name, so "Rock Mix" finds Rock's.
    pub fn list_hue(&self, name: Option<&str>) -> Option<(f64, f64)> {
        let name = name?;
        if name.trim().is_empty() {
            return None;
        }
        let bare = name.trim();
        for candidate in [bare, without(bare, " Mix"), without(bare, " Radio")] {
            let key = song_identity_key(candidate);
            if key.is_empty() {
                continue;
            }
            if let Some(hue) = self.exact.get(&key).or_else(|| self.parts.get(&key)) {
                return Some((*hue, self.list_chroma));
            }
        }
        None
    }
}

fn without<'a>(name: &'a str, suffix: &str) -> &'a str {
    match text::strip_suffix_ignore_case(name, suffix) {
        Some(head) if !head.is_empty() => head,
        _ => name,
    }
}

// STUB(wave 1-B): replace with octo_core::common::song_identity::key
//
// A private copy of SongIdentity.Key (octo/Services/Common/SongIdentity.cs) and what it calls:
// Fold, FoldMixedScriptWords and LowerFold, until the shared module lands.
mod song_identity_stub {
    use std::sync::LazyLock;

    use regex::Regex;
    use unicode_general_category::{GeneralCategory, get_general_category};
    use unicode_normalization::UnicodeNormalization;

    static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("a valid pattern"));
    static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{M}]+").expect("a valid pattern"));

    /// Cyrillic and Greek letters that look Latin, read as Latin inside a word that also has
    /// Latin letters. A word wholly in Cyrillic or Greek is left as it is.
    fn homoglyph(c: char) -> Option<char> {
        Some(match c {
            'А' => 'A',
            'В' => 'B',
            'Е' => 'E',
            'К' => 'K',
            'М' => 'M',
            'Н' => 'H',
            'О' => 'O',
            'Р' => 'P',
            'С' => 'C',
            'Т' => 'T',
            'Х' => 'X',
            'І' => 'I',
            'Ј' => 'J',
            'Ѕ' => 'S',
            'а' => 'a',
            'е' => 'e',
            'о' => 'o',
            'р' => 'p',
            'с' => 'c',
            'у' => 'y',
            'х' => 'x',
            'і' => 'i',
            'ј' => 'j',
            'ѕ' => 's',
            'Α' => 'A',
            'Β' => 'B',
            'Ε' => 'E',
            'Ζ' => 'Z',
            'Η' => 'H',
            'Ι' => 'I',
            'Κ' => 'K',
            'Μ' => 'M',
            'Ν' => 'N',
            'Ο' => 'O',
            'Ρ' => 'P',
            'Τ' => 'T',
            'Υ' => 'Y',
            'Χ' => 'X',
            'ο' => 'o',
            _ => return None,
        })
    }

    fn fold_mixed_script_words(text: &str) -> String {
        WORD.replace_all(text, |caps: &regex::Captures| {
            let word = &caps[0];
            if !word.chars().any(|c| c.is_ascii_alphabetic()) || !word.chars().any(|c| homoglyph(c).is_some())
            {
                return word.to_string();
            }
            word.chars().map(|c| homoglyph(c).unwrap_or(c)).collect()
        })
        .into_owned()
    }

    fn fold(value: &str) -> String {
        if value.trim().is_empty() {
            return String::new();
        }
        let text: String = value.replace('\u{00B4}', "'").nfkc().collect();
        let mapped: String =
            text.chars()
                .map(|ch| match ch {
                    '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '`' => '\'',
                    '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
                    '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                    | '\u{2212}' => '-',
                    '\u{3010}' | '\u{3016}' => '[',
                    '\u{3011}' | '\u{3017}' => ']',
                    '\u{3014}' => '(',
                    '\u{3015}' => ')',
                    '_' => ' ',
                    c if c.is_whitespace() => ' ',
                    c => c,
                })
                .collect();
        WHITESPACE
            .replace_all(&fold_mixed_script_words(&mapped), " ")
            .trim()
            .to_string()
    }

    fn lower_fold(folded: &str) -> String {
        if folded.is_empty() {
            return String::new();
        }
        // ToLowerInvariant maps one character at a time (no final sigma).
        let lower: String = folded.chars().flat_map(char::to_lowercase).collect();
        let lower = lower
            .replace('ß', "ss")
            .replace('æ', "ae")
            .replace('œ', "oe")
            .replace('ø', "o")
            .replace(['đ', 'ð'], "d")
            .replace('ł', "l")
            .replace('þ', "th")
            .replace('ı', "i")
            .replace('&', " and ")
            .replace(" + ", " and ");
        // Only the Latin, Greek and Cyrillic accents. A kana's voicing mark is a different
        // letter, and stripping it would make two Japanese titles one.
        lower
            .nfd()
            .filter(|ch| {
                !matches!(ch, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{FE20}'..='\u{FE2F}')
            })
            .nfc()
            .collect()
    }

    /// The exact key: casefolded, accents stripped, "&" read as "and", and only letters,
    /// digits and combining marks kept. Symbols only when there is nothing else.
    pub(super) fn key(value: &str) -> String {
        let lower = lower_fold(&fold(value));
        if lower.is_empty() {
            return String::new();
        }
        let kept: String = lower
            .chars()
            .filter(|&c| {
                matches!(
                    get_general_category(c),
                    GeneralCategory::UppercaseLetter
                        | GeneralCategory::LowercaseLetter
                        | GeneralCategory::TitlecaseLetter
                        | GeneralCategory::ModifierLetter
                        | GeneralCategory::OtherLetter
                        | GeneralCategory::DecimalNumber
                        | GeneralCategory::NonspacingMark
                        | GeneralCategory::SpacingMark
                )
            })
            .collect();
        if !kept.is_empty() {
            return kept;
        }
        // "!!!" or an emoji: nothing is a letter, so the symbols are the name.
        lower.chars().filter(|c| !c.is_whitespace()).collect()
    }
}

fn song_identity_key(value: &str) -> String {
    song_identity_stub::key(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_identity_stub_keys_names_as_song_identity_does() {
        assert_eq!(song_identity_key("R&B & Soul"), "randbandsoul");
        assert_eq!(song_identity_key("Hip-Hop"), "hiphop");
        assert_eq!(song_identity_key("Ünïcödé Café"), "unicodecafe");
        assert_eq!(song_identity_key("1990s"), "1990s");
        assert_eq!(song_identity_key("!!!"), "!!!");
        assert_eq!(song_identity_key("  "), "");
    }
}
