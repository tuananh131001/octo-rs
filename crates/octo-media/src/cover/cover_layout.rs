//! Where the words of a cover go and how big, as the Octo apps and the design's reference set
//! them (app.winters.octo.covers; tools/cover-art/reference.py). The background and the veil
//! under the words are `cover_backgrounds`' and `cover_veil`'s.

use std::sync::LazyLock;

use regex::Regex;

use super::cover_book::CoverBook;
use super::cover_colours::{self, CoverMusic};
use super::text::{eq_ignore_case, is_letter, is_white_space};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverAlign {
    Left,
    Right,
}

/// The kinds of writing a cover sets differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverScript {
    Latin,
    Wide,
    Tall,
    Emoji,
}

/// How words are set: size in pixels, weight, tracking and line height (shares of the size), most lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverType {
    pub size_px: f32,
    pub weight: i32,
    pub tracking: f32,
    pub line_height: f32,
    pub max_lines: i32,
}

/// What the text engine found: lines, the widest line, the height, and whether words were cut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measured {
    pub lines: i32,
    pub width: f32,
    pub height: f32,
    pub cut: bool,
}

/// The text engine, as the cover's design needs it (C# `ICoverTypesetter`).
pub trait ICoverTypesetter {
    /// The text wrapped in `width`, at most `type_.max_lines` lines.
    fn measure(&self, text: &str, type_: &CoverType, width: f32) -> Measured;

    /// The text on one line.
    fn width_of(&self, text: &str, type_: &CoverType) -> f32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WordsRole {
    Title,
    Line,
    Footer,
}

/// Words on a cover: what, how set, the box they are set in, how they sit in it, their colour,
/// and which block they are.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverWords {
    pub text: String,
    pub type_: CoverType,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub align: CoverAlign,
    pub ink: i32,
    pub measured: Measured,
    pub role: WordsRole,
}

impl CoverWords {
    /// Where the letters themselves are: left, top, right, bottom.
    pub fn inked(&self) -> [f32; 4] {
        let w = self.measured.width.min(self.width);
        let x = if self.align == CoverAlign::Left {
            self.left
        } else {
            self.left + self.width - w
        };
        [x, self.top, x + w, self.top + self.measured.height]
    }
}

/// What a cover is made from: whose it is (its id), the name, the light line, the foot line,
/// and its music's colours.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverSpec {
    pub id: String,
    pub name: String,
    pub line: Option<String>,
    pub footer: Option<String>,
    pub music: Option<CoverMusic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FittedText {
    pub text: String,
    pub type_: CoverType,
    pub measured: Measured,
}

fn trimmed_non_empty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// Where the words go: the name, the light line under it, and the foot line, each white at its opacity.
pub fn words(
    spec: &CoverSpec,
    side: i32,
    setter: &dyn ICoverTypesetter,
    book: &CoverBook,
) -> Vec<CoverWords> {
    let layout = &book.layout;
    let s = side as f32;
    let align = if is_right_to_left(&spec.name) {
        CoverAlign::Right
    } else {
        CoverAlign::Left
    };
    let name = spec.name.trim();
    let white = cover_colours::WHITE;
    // A tiny cover is its artwork only.
    if side < layout.tiny_below_px {
        return Vec::new();
    }
    let margin = cover_colours::round_f32(s * layout.margin) as f32;
    let width = s - 2.0 * margin;
    let mut output = Vec::new();

    // The foot line first, so the name knows how far down it may go.
    let mut floor = s - margin;
    let footer_text =
        trimmed_non_empty(spec.footer.as_deref()).filter(|_| side >= layout.footer.show_from_px);
    if let Some(footer_text) = footer_text {
        let f = &layout.footer;
        let look = CoverType {
            size_px: 0.0,
            weight: f.weight,
            tracking: f.tracking,
            line_height: line_height(script_of(footer_text), f.line_height),
            max_lines: 1,
        };
        let size = f.min_px.max(s * f.size);
        let fit = fit_text(footer_text, width, s * 0.2, 1, f.min_px, size, &look, setter);
        let top = s - s * f.bottom - fit.measured.height;
        output.push(CoverWords {
            text: footer_text.to_string(),
            type_: fit.type_,
            left: margin,
            top,
            width,
            align,
            ink: cover_colours::alpha(white, f.opacity),
            measured: fit.measured,
            role: WordsRole::Footer,
        });
        floor = top - s * 0.04;
    }
    if name.is_empty() {
        return output;
    }

    let t = &layout.title;
    let title_top = s * t.top;
    let line_text = trimmed_non_empty(spec.line.as_deref())
        .filter(|_| side >= layout.line.show_from_px && !says_what_it_is(name));
    let line_room = if line_text.is_none() {
        0.0
    } else {
        (s * t.wrap_size).max(t.wrap_min_px) * layout.line.share_of_title * layout.line.line_height
    };
    let room = floor - title_top - line_room;
    let title_look = CoverType {
        size_px: 0.0,
        weight: t.weight,
        tracking: t.tracking,
        line_height: line_height(script_of(name), t.line_height),
        max_lines: 1,
    };
    let min_px = t.min_px.max(s * t.min_size);
    let one_line_least = min_px.max((s * t.one_line_down_to).max(t.one_line_min_px));
    let wrap_most = min_px.max((s * t.wrap_size).max(t.wrap_min_px.min(s * t.size)));
    let title = (if one_line_least <= s * t.size {
        fit_or_null(
            name,
            width,
            room,
            1,
            one_line_least,
            s * t.size,
            &title_look,
            setter,
        )
    } else {
        None
    })
    .unwrap_or_else(|| {
        fit_text(
            name,
            width,
            room,
            t.max_lines,
            min_px,
            wrap_most,
            &title_look,
            setter,
        )
    });
    let title_size = title.type_.size_px;
    let title_height = title.measured.height;
    output.push(CoverWords {
        text: name.to_string(),
        type_: title.type_,
        left: margin,
        top: title_top,
        width,
        align,
        ink: white,
        measured: title.measured,
        role: WordsRole::Title,
    });

    if let Some(line_text) = line_text {
        let l = &layout.line;
        let line_top = title_top + title_height;
        let most = l.min_px.max(title_size * l.share_of_title);
        let line_look = CoverType {
            size_px: 0.0,
            weight: l.weight,
            tracking: l.tracking,
            line_height: line_height(script_of(line_text), l.line_height),
            max_lines: 1,
        };
        let fit = fit_text(
            line_text,
            width,
            s,
            1,
            l.min_px.min(most),
            most,
            &line_look,
            setter,
        );
        if line_top + fit.measured.height <= floor {
            output.push(CoverWords {
                text: line_text.to_string(),
                type_: fit.type_,
                left: margin,
                top: line_top,
                width,
                align,
                ink: white,
                measured: fit.measured,
                role: WordsRole::Line,
            });
        }
    }
    output
}

/// The largest whole-pixel size from `min_px` to `max_px` at which the text fits the box in at
/// most `max_lines` lines with no word broken, or None when even the smallest does not.
pub fn fit_or_null(
    text: &str,
    width: f32,
    height: f32,
    max_lines: i32,
    min_px: f32,
    max_px: f32,
    look: &CoverType,
    setter: &dyn ICoverTypesetter,
) -> Option<FittedText> {
    let runs = unbreakable_runs(text);
    let type_at = |size: f32| CoverType {
        size_px: size,
        max_lines,
        ..*look
    };
    let fits = |size: f32| -> Option<Measured> {
        let type_ = type_at(size);
        if runs.iter().any(|run| setter.width_of(run, &type_) > width) {
            return None;
        }
        let measured = setter.measure(text, &type_, width);
        (!measured.cut && measured.lines <= max_lines && measured.height <= height).then_some(measured)
    };
    let mut low = min_px.floor().max(1.0);
    let mut high = max_px.floor().max(low);
    if let Some(at_high) = fits(high) {
        return Some(FittedText {
            text: text.to_string(),
            type_: type_at(high),
            measured: at_high,
        });
    }
    let mut best = fits(low)?;
    while high - low > 1.0 {
        let mid = ((low + high) / 2.0).floor();
        if let Some(measured) = fits(mid) {
            low = mid;
            best = measured;
        } else {
            high = mid;
        }
    }
    Some(FittedText {
        text: text.to_string(),
        type_: type_at(low),
        measured: best,
    })
}

/// As [`fit_or_null`], but when nothing fits: the smallest size, as many lines as the box
/// holds, the rest cut.
pub fn fit_text(
    text: &str,
    width: f32,
    height: f32,
    max_lines: i32,
    min_px: f32,
    max_px: f32,
    look: &CoverType,
    setter: &dyn ICoverTypesetter,
) -> FittedText {
    if let Some(fitted) = fit_or_null(text, width, height, max_lines, min_px, max_px, look, setter) {
        return fitted;
    }
    let size = min_px.floor().max(1.0);
    let lines = ((height / (size * look.line_height)) as i32).clamp(1, max_lines);
    let type_ = CoverType {
        size_px: size,
        max_lines: lines,
        ..*look
    };
    FittedText {
        text: text.to_string(),
        type_,
        measured: setter.measure(text, &type_, width),
    }
}

// ---------------------------------------------------------------- writing

fn is_emoji(cp: u32) -> bool {
    matches!(cp, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF)
}

pub(crate) fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x11FF | 0x2E80..=0x2FDF | 0x3040..=0x30FF | 0x3100..=0x312F
        | 0x3130..=0x318F | 0x31A0..=0x31BF | 0x31F0..=0x31FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7FF | 0xF900..=0xFAFF
        | 0xFF66..=0xFF9F | 0x20000..=0x3FFFF | 0x3005 | 0x3006 | 0x3007)
}

fn is_spaced(cp: u32) -> bool {
    matches!(cp,
        0x0041..=0x024F | 0x1E00..=0x1EFF | 0x0370..=0x03FF | 0x1F00..=0x1FFF
        | 0x0400..=0x052F | 0x0530..=0x058F | 0x10A0..=0x10FF | 0x2C00..=0x2C7F
        | 0xA720..=0xA7FF | 0xFF21..=0xFF5A)
}

fn is_rtl_letter(cp: u32) -> bool {
    matches!(cp,
        0x0590..=0x05FF | 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F
        | 0x0780..=0x07BF | 0x07C0..=0x07FF | 0x0860..=0x08FF | 0xFB1D..=0xFDFF
        | 0xFE70..=0xFEFF | 0x1EE00..=0x1EEFF)
}

/// The writing most of the text is in; digits, spaces and marks do not count.
pub fn script_of(text: &str) -> CoverScript {
    let (mut latin, mut wide, mut tall, mut emoji) = (0, 0, 0, 0);
    for rune in text.chars() {
        let cp = rune as u32;
        if is_emoji(cp) {
            emoji += 1;
        } else if !is_letter(rune) {
        } else if is_wide(cp) {
            wide += 1;
        } else if is_spaced(cp) {
            latin += 1;
        } else {
            tall += 1;
        }
    }
    let most = latin.max(wide).max(tall.max(emoji));
    if most == 0 || most == latin {
        CoverScript::Latin
    } else if most == wide {
        CoverScript::Wide
    } else if most == tall {
        CoverScript::Tall
    } else {
        CoverScript::Emoji
    }
}

/// Whether the text reads right to left: its first letter decides.
pub fn is_right_to_left(text: &str) -> bool {
    text.chars()
        .find(|&c| is_letter(c))
        .is_some_and(|c| is_rtl_letter(c as u32))
}

/// The space between lines for this writing: room for marks in the scripts that have them.
pub fn line_height(script: CoverScript, wanted: f32) -> f32 {
    match script {
        CoverScript::Latin => wanted,
        CoverScript::Wide | CoverScript::Emoji => wanted.max(1.15),
        CoverScript::Tall => wanted.max(1.4),
    }
}

const KIND_WORDS: &[&str] = &[
    "Mix",
    "Mixes",
    "Radio",
    "Radios",
    "Station",
    "Stations",
    "Playlist",
    "Playlists",
];

/// .NET's `\w`: letters, non-spacing marks, decimal digits and connector punctuation.
static NOT_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\p{L}\p{Mn}\p{Nd}\p{Pc}]+").expect("a valid pattern"));

/// Whether the name's last word already says what kind of list it is, so no light line repeats it.
pub fn says_what_it_is(name: &str) -> bool {
    NOT_WORD
        .split(name.trim())
        .filter(|w| !w.is_empty())
        .last()
        .is_some_and(|last| KIND_WORDS.iter().any(|kind| eq_ignore_case(kind, last)))
}

/// The pieces that cannot be broken across lines: words between spaces; wide characters each stand alone.
pub fn unbreakable_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut word = String::new();
    for rune in text.chars() {
        if is_white_space(rune) {
            if !word.is_empty() {
                runs.push(std::mem::take(&mut word));
            }
        } else if is_wide(rune as u32) {
            if !word.is_empty() {
                runs.push(std::mem::take(&mut word));
            }
            runs.push(rune.to_string());
        } else {
            word.push(rune);
        }
    }
    if !word.is_empty() {
        runs.push(word);
    }
    runs
}
