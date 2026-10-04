//! The cover type: Inter Display in the three weights the design names, shipped inside the app
//! so every box sets the same letters, with the system's fonts behind it for writing Inter does
//! not cover (Chinese, Japanese, Korean, Arabic, Hebrew, emoji). The Docker image installs Noto
//! CJK and Symbola for that; DejaVu was already there.
//!
//! C# set type with SixLabors.Fonts; this port shapes with cosmic-text (harfrust) and draws
//! the glyph outlines (skrifa) with tiny-skia. Fonts are named as SixLabors named them: by the
//! name table's family name (name ID 1), so "Inter Display SemiBold", "Noto Sans CJK SC".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use cosmic_text::skrifa::instance::{LocationRef, Size};
use cosmic_text::skrifa::outline::{DrawSettings, OutlinePen};
use cosmic_text::skrifa::raw::TableProvider;
use cosmic_text::skrifa::raw::tables::os2::SelectionFlags;
use cosmic_text::skrifa::{FontRef, GlyphId, MetadataProvider, string::StringId};
use cosmic_text::{
    Attrs, AttrsList, Fallback, Family, FontSystem, Hinting, ShapeLine, Shaping, Stretch, Style, Weight,
    Wrap, fontdb,
};
use parking_lot::Mutex;
use unicode_segmentation::UnicodeSegmentation;

use super::cover_book::CoverBook;
use super::cover_layout::{self, CoverAlign, CoverType, CoverWords, ICoverTypesetter, Measured};
use super::design;
use super::text::{is_letter, is_white_space};

/// Tried in this order for letters Inter lacks; only the installed ones count.
const FALLBACK_NAMES: &[&str] = &[
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK KR",
    "Noto Sans CJK TC",
    "Microsoft YaHei",
    "Yu Gothic",
    "Malgun Gothic",
    "Microsoft JhengHei",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    "Segoe UI",
    "DejaVu Sans",
    "Noto Sans",
    "Segoe UI Symbol",
    "Segoe UI Emoji",
    "Symbola",
    "Noto Emoji",
];

/// Where SixLabors.Fonts' `SystemFonts` looks on Linux, searched recursively.
fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".fonts"));
    }
    dirs.push(PathBuf::from("/usr/local/share/fonts"));
    dirs.push(PathBuf::from("/usr/share/fonts"));
    dirs
}

/// One face, and how to ask cosmic-text for exactly it.
#[derive(Clone, Debug)]
struct Face {
    id: fontdb::ID,
    /// fontdb's (typographic) family name, which cosmic-text matches on.
    query_family: String,
    weight: Weight,
    stretch: Stretch,
    style: Style,
}

/// A family as SixLabors grouped them: the faces sharing a name ID 1.
#[derive(Clone, Debug)]
struct FontFamily {
    name: String,
    regular: Face,
    bold: Option<Face>,
}

/// The font to set some text in: a family, in bold or not, and the families behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontChoice {
    /// The family's name (name ID 1), as C#'s `Font.Family.Name`.
    pub family: String,
    pub bold: bool,
    /// The families tried, in order, for letters it lacks.
    pub fallbacks: Vec<String>,
}

/// The fallback list cosmic-text walks for letters the chosen font lacks: Inter, then the
/// installed system families, in the design's order.
struct CoverFallback {
    common: &'static [&'static str],
}

impl Fallback for CoverFallback {
    fn common_fallback(&self) -> &[&'static str] {
        self.common
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn script_fallback(&self, _script: unicode_script::Script, _locale: &str) -> &[&'static str] {
        &[]
    }
}

struct Fonts {
    system: FontSystem,
    /// The shipped Inter weights, by design file name.
    inter: HashMap<String, FontFamily>,
    /// The installed fallback families, in `FALLBACK_NAMES` order.
    fallbacks: Vec<FontFamily>,
    choices: HashMap<(String, i32), (FontFamily, bool)>,
}

static FONTS: LazyLock<Mutex<Fonts>> = LazyLock::new(|| Mutex::new(Fonts::load()));

/// The family name SixLabors knows a face by: its name table's family (name ID 1).
fn legacy_family(data: &[u8], index: u32) -> Option<String> {
    let font = FontRef::from_index(data, index).ok()?;
    let name = font.localized_strings(StringId::FAMILY_NAME).english_or_first()?;
    Some(name.to_string())
}

/// Whether a face is bold, italic, by its OS/2 selection flags (as SixLabors reads its style).
fn face_style(data: &[u8], index: u32) -> (bool, bool) {
    let Ok(font) = FontRef::from_index(data, index) else {
        return (false, false);
    };
    match font.os2() {
        Ok(os2) => {
            let flags = os2.fs_selection();
            (
                flags.contains(SelectionFlags::BOLD),
                flags.contains(SelectionFlags::ITALIC),
            )
        }
        Err(_) => (false, false),
    }
}

fn face_of(db: &fontdb::Database, id: fontdb::ID) -> Option<Face> {
    let info = db.face(id)?;
    Some(Face {
        id,
        query_family: info.families.first()?.0.clone(),
        weight: info.weight,
        stretch: info.stretch,
        style: info.style,
    })
}

impl Fonts {
    fn load() -> Fonts {
        let mut db = fontdb::Database::new();
        for dir in system_font_dirs() {
            if dir.is_dir() {
                db.load_fonts_dir(&dir);
            }
        }
        // Group the system faces by name ID 1, keeping the regular and bold face of each
        // family the design falls back to.
        // In path order, so that of two copies of a face the same one counts on every start.
        let mut faces: Vec<(String, u32, fontdb::ID)> = db
            .faces()
            .map(|f| {
                let path = match &f.source {
                    fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                        path.to_string_lossy().into_owned()
                    }
                    fontdb::Source::Binary(_) => String::new(),
                };
                (path, f.index, f.id)
            })
            .collect();
        faces.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        let mut wanted: HashMap<String, (Option<fontdb::ID>, Option<fontdb::ID>)> = HashMap::new();
        for (_, _, id) in faces {
            let found = db.with_face_data(id, |data, index| {
                let name = legacy_family(data, index)?;
                FALLBACK_NAMES
                    .contains(&name.as_str())
                    .then(|| (name, face_style(data, index)))
            });
            match found.flatten() {
                Some((name, (bold, italic))) if !italic => {
                    let slot = wanted.entry(name).or_default();
                    if bold && slot.1.is_none() {
                        slot.1 = Some(id);
                    } else if !bold && slot.0.is_none() {
                        slot.0 = Some(id);
                    } else {
                        db.remove_face(id);
                    }
                }
                _ => db.remove_face(id),
            }
        }

        // The shipped Inter weights.
        let book = CoverBook::shipped();
        let mut inter_ids = HashMap::new();
        for file in [&book.fonts.title, &book.fonts.line, &book.fonts.footer] {
            if inter_ids.contains_key(file.as_str()) {
                continue;
            }
            let bytes = design::font(file).expect("every cover font the design names is embedded");
            let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(bytes)));
            inter_ids.insert(file.clone(), *ids.first().expect("an embedded cover font parses"));
        }

        let mut inter = HashMap::new();
        for (file, id) in &inter_ids {
            let face = face_of(&db, *id).expect("an embedded cover font loads");
            let name = db
                .with_face_data(*id, legacy_family)
                .flatten()
                .unwrap_or_else(|| face.query_family.clone());
            inter.insert(
                file.clone(),
                FontFamily {
                    name,
                    regular: face,
                    bold: None,
                },
            );
        }

        let mut fallbacks = Vec::new();
        for name in FALLBACK_NAMES {
            let Some((regular, bold)) = wanted.get(*name) else {
                continue;
            };
            // A family with only a bold face is no use as a regular one, as SixLabors could
            // not create its regular font either.
            let Some(regular) = regular.and_then(|id| face_of(&db, id)) else {
                continue;
            };
            let bold = bold.and_then(|id| face_of(&db, id));
            fallbacks.push(FontFamily {
                name: (*name).to_string(),
                regular,
                bold,
            });
        }

        // cosmic-text's own fallback walk: Inter, then the installed families. The names live
        // as long as the process (the font system is made once).
        let mut common: Vec<&'static str> = Vec::new();
        let inter_query = inter
            .values()
            .next()
            .map(|f| f.regular.query_family.clone())
            .unwrap_or_default();
        common.push(Box::leak(inter_query.into_boxed_str()));
        for family in &fallbacks {
            let query: &'static str = Box::leak(family.regular.query_family.clone().into_boxed_str());
            if !common.contains(&query) {
                common.push(query);
            }
        }
        let common: &'static [&'static str] = Box::leak(common.into_boxed_slice());
        let system = FontSystem::new_with_locale_and_db_and_fallback(
            "en-US".to_string(),
            db,
            CoverFallback { common },
        );
        Fonts {
            system,
            inter,
            fallbacks,
            choices: HashMap::new(),
        }
    }

    /// The design's file for a weight: the name's, the light line's, or the foot line's.
    fn for_weight(&self, weight: i32) -> &FontFamily {
        let book = CoverBook::shipped();
        let file = if weight == book.layout.title.weight {
            &book.fonts.title
        } else if weight == book.layout.line.weight {
            &book.fonts.line
        } else {
            &book.fonts.footer
        };
        &self.inter[file]
    }

    /// Whether the face draws the character itself, not its empty box.
    fn has(&self, face: &Face, cp: char) -> bool {
        self.system
            .db()
            .with_face_data(face.id, |data, index| {
                FontRef::from_index(data, index)
                    .ok()
                    .and_then(|font| font.charmap().map(cp))
                    .is_some_and(|glyph| glyph != GlyphId::NOTDEF)
            })
            .unwrap_or(false)
    }

    /// The font to set `text` in: Inter, unless it holds letters Inter does not have; then the
    /// installed font that covers most of them, in bold for the name so a Japanese name is as
    /// heavy as an English one, with Inter behind it.
    fn choose(&mut self, text: &str, weight: i32) -> (FontFamily, bool) {
        let key = (text.to_string(), weight);
        if let Some(known) = self.choices.get(&key) {
            return known.clone();
        }
        if self.choices.len() >= 20_000 {
            self.choices.clear();
        }
        let inter = self.for_weight(weight).clone();
        let missing: Vec<char> = text
            .chars()
            .filter(|&c| is_letter(c) && !self.has(&inter.regular, c))
            .collect();
        let chosen = if missing.is_empty() {
            (inter, false)
        } else {
            let mut best: Option<&FontFamily> = None;
            let mut best_count = 0;
            for family in &self.fallbacks {
                let count = missing
                    .iter()
                    .filter(|&&cp| self.has(&family.regular, cp))
                    .count();
                if count > best_count {
                    (best, best_count) = (Some(family), count);
                }
            }
            match best {
                None => (inter, false),
                Some(chosen) => {
                    let bold = weight == CoverBook::shipped().layout.title.weight && chosen.bold.is_some();
                    (chosen.clone(), bold)
                }
            }
        };
        self.choices.insert(key, chosen.clone());
        chosen
    }

    fn fallback_names(&self, chosen: &FontFamily, weight: i32) -> Vec<String> {
        let inter = self.for_weight(weight);
        if chosen.name == inter.name {
            return self.fallbacks.iter().map(|f| f.name.clone()).collect();
        }
        std::iter::once(inter.name.clone())
            .chain(
                self.fallbacks
                    .iter()
                    .filter(|f| f.name != chosen.name)
                    .map(|f| f.name.clone()),
            )
            .collect()
    }

    /// Shapes one line of text in its chosen font at `size` pixels.
    fn shape(&mut self, text: &str, weight: i32, size: f32) -> Shaped {
        let (family, bold) = self.choose(text, weight);
        let face = if bold {
            family.bold.clone().unwrap_or(family.regular.clone())
        } else {
            family.regular.clone()
        };
        let attrs = Attrs::new()
            .family(Family::Name(&face.query_family))
            .weight(face.weight)
            .stretch(face.stretch)
            .style(face.style);
        let line = ShapeLine::new(
            &mut self.system,
            text,
            &AttrsList::new(&attrs),
            Shaping::Advanced,
            8,
        );
        let layout = line.layout(size, None, Wrap::None, None, None, Hinting::Disabled);
        let mut glyphs = Vec::new();
        let mut width = 0.0f32;
        for laid in &layout {
            width = width.max(laid.w);
            for glyph in &laid.glyphs {
                glyphs.push(PlacedGlyph {
                    font: glyph.font_id,
                    glyph: glyph.glyph_id,
                    size: glyph.font_size,
                    x: glyph.x + glyph.font_size * glyph.x_offset,
                    y: glyph.y - glyph.font_size * glyph.y_offset,
                });
            }
        }
        Shaped { width, glyphs, face }
    }

    /// The chosen face's ascent and descent (both positive) in pixels at `size`, from the line
    /// metrics SixLabors used: OS/2 typographic ones when the font asks for them, else hhea.
    fn vertical_metrics(&self, face: &Face, size: f32) -> (f32, f32) {
        self.metrics_of(face.id, size)
    }

    fn metrics_of(&self, id: fontdb::ID, size: f32) -> (f32, f32) {
        self.system
            .db()
            .with_face_data(id, |data, index| {
                let font = FontRef::from_index(data, index).ok()?;
                let metrics = font.metrics(Size::new(size), LocationRef::default());
                Some((metrics.ascent, -metrics.descent))
            })
            .flatten()
            .unwrap_or((size * 0.8, size * 0.2))
    }

    /// How far below the top of its line SixLabors set a baseline, with the line's fonts'
    /// largest ascent and descent centred in a line one size tall (VerticalAlignment.Top).
    fn top_to_baseline(&self, ids: impl IntoIterator<Item = fontdb::ID>, size: f32) -> f32 {
        let (mut ascent, mut descent) = (f32::MIN, f32::MIN);
        for id in ids {
            let (a, d) = self.metrics_of(id, size);
            ascent = ascent.max(a);
            descent = descent.max(d);
        }
        if ascent == f32::MIN {
            return 0.0;
        }
        (size + ascent - descent) / 2.0
    }

    /// The face that draws a capital H for text set in `face`: that face, or the first family
    /// behind it that has one (Inter first).
    fn h_face(&self, face: &Face, family: &str, weight: i32) -> fontdb::ID {
        if self.has(face, 'H') {
            return face.id;
        }
        let inter = self.for_weight(weight);
        std::iter::once(inter)
            .chain(self.fallbacks.iter().filter(|f| f.name != family))
            .map(|f| &f.regular)
            .find(|f| self.has(f, 'H'))
            .map_or(face.id, |f| f.id)
    }

    /// The outlines of shaped glyphs as one path, the line's left edge at `x` and its baseline
    /// at `baseline`.
    ///
    /// C# drew each line from the top of its box, so that a capital H's foot, measured in the
    /// same font settings, sat on the baseline. Where the H comes from another font than the
    /// text (an Arabic or Hebrew name: Noto has no Latin capitals, so Inter draws the H), the
    /// two boxes differ, and the text sits that much off the baseline. The port keeps that.
    fn outline(
        &self,
        shaped: &Shaped,
        family: &str,
        weight: i32,
        x: f32,
        baseline: f32,
    ) -> Option<tiny_skia::Path> {
        let size = shaped.glyphs.first().map_or(0.0, |g| g.size);
        let mut fonts: Vec<fontdb::ID> = shaped.glyphs.iter().map(|g| g.font).collect();
        fonts.dedup();
        let foot = self.top_to_baseline([self.h_face(&shaped.face, family, weight)], size);
        let baseline = baseline - foot + self.top_to_baseline(fonts, size);
        let mut pen = PathPen {
            builder: tiny_skia::PathBuilder::new(),
            x: 0.0,
            y: 0.0,
        };
        for glyph in &shaped.glyphs {
            pen.x = x + glyph.x;
            pen.y = baseline + glyph.y;
            self.system.db().with_face_data(glyph.font, |data, index| {
                let Ok(font) = FontRef::from_index(data, index) else {
                    return;
                };
                let Some(outline) = font.outline_glyphs().get(GlyphId::new(u32::from(glyph.glyph))) else {
                    return;
                };
                let _ = outline.draw(
                    DrawSettings::unhinted(Size::new(glyph.size), LocationRef::default()),
                    &mut pen,
                );
            });
        }
        pen.builder.finish()
    }
}

struct PlacedGlyph {
    font: fontdb::ID,
    glyph: u16,
    size: f32,
    /// Offsets from the line's left edge and baseline, y down.
    x: f32,
    y: f32,
}

struct Shaped {
    width: f32,
    glyphs: Vec<PlacedGlyph>,
    face: Face,
}

/// Turns font outlines (y up, origin on the baseline) into a path on the cover (y down).
struct PathPen {
    builder: tiny_skia::PathBuilder,
    x: f32,
    y: f32,
}

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(self.x + x, self.y - y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(self.x + x, self.y - y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder
            .quad_to(self.x + cx0, self.y - cy0, self.x + x, self.y - y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.builder.cubic_to(
            self.x + cx0,
            self.y - cy0,
            self.x + cx1,
            self.y - cy1,
            self.x + x,
            self.y - y,
        );
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// The installed families behind Inter, in the order they are tried.
pub fn fallbacks() -> Vec<String> {
    FONTS.lock().fallbacks.iter().map(|f| f.name.clone()).collect()
}

/// The design's family for a weight: the name's, the light line's, or the foot line's.
pub fn for_weight(weight: i32) -> String {
    FONTS.lock().for_weight(weight).name.clone()
}

/// The font to set `text` in at this weight (C# `CoverFonts.For`, less the size).
pub fn for_text(text: &str, weight: i32) -> FontChoice {
    let mut fonts = FONTS.lock();
    let (family, bold) = fonts.choose(text, weight);
    let fallbacks = fonts.fallback_names(&family, weight);
    FontChoice {
        family: family.name,
        bold,
        fallbacks,
    }
}

/// Whether the family (by name) draws the character itself, not its empty box. Its regular
/// face is asked, as C# asked `family.CreateFont(size)`.
pub fn has(family: &str, cp: char) -> bool {
    let fonts = FONTS.lock();
    let face = fonts
        .inter
        .values()
        .chain(fonts.fallbacks.iter())
        .find(|f| f.name == family)
        .map(|f| f.regular.clone());
    face.is_some_and(|face| fonts.has(&face, cp))
}

static ADVANCES: LazyLock<Mutex<HashMap<(String, i32, u32), f32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The width of the text on one line. Remembered, since fitting asks the same widths often.
pub fn advance(text: &str, type_: &CoverType) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let key = (text.to_string(), type_.weight, type_.size_px.to_bits());
    if let Some(known) = ADVANCES.lock().get(&key) {
        return *known;
    }
    let width = FONTS.lock().shape(text, type_.weight, type_.size_px).width;
    let mut advances = ADVANCES.lock();
    if advances.len() >= 50_000 {
        advances.clear();
    }
    advances.insert(key, width);
    width
}

/// One line of a block of words, and its width.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    pub width: f32,
}

/// Where a line's letters go: its left edge and baseline, and the font it is set in.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedLine {
    pub text: String,
    pub weight: i32,
    pub size: f32,
    pub family: String,
    pub bold: bool,
    pub x: f32,
    pub baseline: f32,
}

/// The design's text engine: lines broken between words (and between Chinese, Japanese and
/// Korean characters), each line's height the size times the line height with the letters
/// centred in it, and an ellipsis where words are cut, as the apps set them.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoverTypesetter;

impl ICoverTypesetter for CoverTypesetter {
    fn measure(&self, text: &str, type_: &CoverType, width: f32) -> Measured {
        let (lines, cut) = self.lines(text, type_, width);
        let widest = lines.iter().map(|line| line.width).fold(0.0f32, f32::max);
        Measured {
            lines: lines.len() as i32,
            width: widest,
            height: lines.len() as f32 * type_.size_px * type_.line_height,
            cut,
        }
    }

    fn width_of(&self, text: &str, type_: &CoverType) -> f32 {
        advance(text, type_)
    }
}

impl CoverTypesetter {
    /// The text broken into at most `type_.max_lines` lines of `width`.
    pub fn lines(&self, text: &str, type_: &CoverType, width: f32) -> (Vec<Line>, bool) {
        let pieces = pieces(text);
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        for (piece, spaced) in &pieces {
            let candidate = if current.is_empty() {
                piece.clone()
            } else {
                format!("{current}{}{piece}", if *spaced { " " } else { "" })
            };
            if current.is_empty() || advance(&candidate, type_) <= width {
                current = candidate;
                continue;
            }
            lines.push(std::mem::replace(&mut current, piece.clone()));
        }
        if !current.is_empty() {
            lines.push(current);
        }

        let max = type_.max_lines.max(1) as usize;
        let mut cut = lines.len() > max;
        if cut {
            let rest = lines[max - 1..].join(" ");
            lines.truncate(max - 1);
            lines.push(ellipsize(&rest, type_, width));
        }
        // A single word wider than the line is cut too.
        for line in lines.iter_mut() {
            if advance(line, type_) <= width {
                continue;
            }
            *line = ellipsize(line, type_, width);
            cut = true;
        }
        let lines = lines
            .into_iter()
            .map(|line| {
                let width = advance(&line, type_);
                Line { text: line, width }
            })
            .collect();
        (lines, cut)
    }

    /// Where each line's letters go: its left edge and baseline. A line is the size times the
    /// line height tall, with the font's ascent and descent centred in it.
    pub fn place(&self, words: &CoverWords) -> Vec<PlacedLine> {
        let type_ = &words.type_;
        let (lines, _) = self.lines(&words.text, type_, words.width);
        let line_height = type_.size_px * type_.line_height;
        let mut placed = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            let mut fonts = FONTS.lock();
            let (family, bold) = fonts.choose(&line.text, type_.weight);
            let face = if bold {
                family.bold.clone().unwrap_or(family.regular.clone())
            } else {
                family.regular.clone()
            };
            let (ascent, descent) = fonts.vertical_metrics(&face, type_.size_px);
            drop(fonts);
            let baseline =
                words.top + i as f32 * line_height + (line_height - (ascent + descent)) / 2.0 + ascent;
            let x = if words.align == CoverAlign::Left {
                words.left
            } else {
                words.left + words.width - line.width
            };
            placed.push(PlacedLine {
                text: line.text.clone(),
                weight: type_.weight,
                size: type_.size_px,
                family: family.name,
                bold,
                x,
                baseline,
            });
        }
        placed
    }

    /// The outline of a placed line, ready to fill.
    pub(crate) fn outline(&self, line: &PlacedLine) -> Option<tiny_skia::Path> {
        let mut fonts = FONTS.lock();
        let shaped = fonts.shape(&line.text, line.weight, line.size);
        fonts.outline(&shaped, &line.family, line.weight, line.x, line.baseline)
    }
}

fn ellipsize(text: &str, type_: &CoverType, width: f32) -> String {
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    for keep in (1..=graphemes.len()).rev() {
        let candidate = format!("{}…", graphemes[..keep].concat().trim_end());
        if advance(&candidate, type_) <= width {
            return candidate;
        }
    }
    "…".to_string()
}

/// The unbreakable pieces of the text, each marked with whether a space came before it.
fn pieces(text: &str) -> Vec<(String, bool)> {
    let mut pieces = Vec::new();
    let mut word = String::new();
    let mut word_spaced = false;
    let mut space = false;
    for rune in text.chars() {
        if is_white_space(rune) {
            if !word.is_empty() {
                pieces.push((std::mem::take(&mut word), word_spaced));
            }
            space = true;
        } else if cover_layout::is_wide(rune as u32) {
            if !word.is_empty() {
                pieces.push((std::mem::take(&mut word), word_spaced));
            }
            pieces.push((rune.to_string(), space));
            space = false;
        } else {
            if word.is_empty() {
                word_spaced = space;
                space = false;
            }
            word.push(rune);
        }
    }
    if !word.is_empty() {
        pieces.push((word, word_spaced));
    }
    pieces
}
