//! Paints a composed cover: its painted background at the cover's size, turned as the cover
//! says, the colour-keeping veil under the words, then the words in white at their lines' baselines.

use anyhow::anyhow;
use image::RgbImage;

use super::cover_backgrounds;
use super::cover_book::CoverBook;
use super::cover_colours;
use super::cover_fonts::CoverTypesetter;
use super::cover_layout::CoverWords;
use super::cover_veil;

/// A cover ready to paint: its side in pixels, its background (an index into the library), how
/// that background is turned (0 to 7: quarter turns clockwise, then a mirror from 4 up), and its words.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverArt {
    pub side: i32,
    pub background: usize,
    pub orientation: i32,
    pub words: Vec<CoverWords>,
}

/// Paints the cover. With `draw_words` false, the veiled background alone.
pub fn paint(
    book: &CoverBook,
    art: &CoverArt,
    setter: &CoverTypesetter,
    draw_words: bool,
) -> anyhow::Result<RgbImage> {
    let mut image = cover_backgrounds::load(book, art.background, art.side)?;
    cover_backgrounds::turn(&mut image, art.orientation);
    cover_veil::apply(
        &mut image,
        &cover_veil::regions(book, &art.words, art.side),
        &book.veil,
    );
    if !draw_words {
        return Ok(image);
    }

    let mut lines = Vec::new();
    for words in &art.words {
        let ink = cover_colours::a(words.ink) as u8;
        // The baseline sits where a capital H's foot is: the outlines are drawn from it.
        for placed in setter.place(words) {
            lines.push((placed, ink));
        }
    }
    let outlines: Vec<(tiny_skia::Path, u8)> = lines
        .iter()
        .filter_map(|(placed, ink)| setter.outline(placed).map(|path| (path, *ink)))
        .collect();
    if outlines.is_empty() {
        return Ok(image);
    }

    // The words are white over the colours, at their opacity, with eight-or-more steps of edge
    // smoothing (tiny-skia's anti-aliasing; C# asked ImageSharp for eight).
    let (width, height) = image.dimensions();
    let rgba: Vec<u8> = image
        .pixels()
        .flat_map(|p| [p.0[0], p.0[1], p.0[2], 255])
        .collect();
    let size = tiny_skia::IntSize::from_wh(width, height).ok_or_else(|| anyhow!("an empty cover"))?;
    let mut pixmap =
        tiny_skia::Pixmap::from_vec(rgba, size).ok_or_else(|| anyhow!("a cover too large to paint"))?;
    for (path, ink) in &outlines {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(255, 255, 255, *ink);
        paint.anti_alias = true;
        pixmap.fill_path(
            path,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }
    // The background is opaque, so the premultiplied colours are the colours.
    let rgb: Vec<u8> = pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    RgbImage::from_raw(width, height, rgb).ok_or_else(|| anyhow!("a cover lost its pixels"))
}
