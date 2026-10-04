//! The library of painted backgrounds: which one a list gets, and its pixels at a size.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, anyhow};
use image::{RgbImage, imageops};
use parking_lot::Mutex;

use super::cover_book::{Background, BackgroundRule, CoverBook};
use super::cover_colours::{self, CoverMusic};
use super::design;

/// The background for a list, by cover-design.json "background": with its music's colour,
/// the nearest few (ties in file order) and among them the one the list's id picks; without,
/// or with music too dull to say much (chroma under lowChromaAsGrey), any one, picked by the
/// id alone. The same list with the same music always gets the same.
pub fn choose(book: &CoverBook, music: Option<&CoverMusic>, id: &str) -> usize {
    let pick = (cover_colours::cover_pick(id) as u64 >> 7) as usize;
    let all = &book.backgrounds;
    let music = match music {
        Some(music) if music.chroma >= book.background_choice.low_chroma_as_grey => music,
        _ => return pick % all.len(),
    };
    let distances: Vec<f64> = all
        .iter()
        .map(|b| distance(b, music, &book.background_choice))
        .collect();
    let mut near: Vec<usize> = (0..all.len()).collect();
    // A stable sort, as LINQ's OrderBy: ties stay in file order.
    near.sort_by(|&x, &y| distances[x].total_cmp(&distances[y]));
    // At least one, so a design asking for none cannot leave nothing to pick from.
    near.truncate(book.background_choice.nearest.max(1) as usize);
    near[pick % near.len()]
}

/// How a list's background is turned, so lists that share a background still look apart:
/// v = (coverPick(id) >>> shift) mod count, from cover-design.json "orientation".
pub fn orientation(book: &CoverBook, id: &str) -> i32 {
    let rule = &book.background_choice.orientation;
    ((cover_colours::cover_pick(id) as u64 >> rule.shift) % rule.count as u64) as i32
}

/// Turns a background in place: (v mod 4) quarter turns clockwise, then a mirror left to right when v >= 4.
pub fn turn(image: &mut RgbImage, orientation: i32) {
    let turns = orientation % 4;
    let mirror = orientation >= 4;
    if turns == 0 && !mirror {
        return;
    }
    match turns {
        1 => *image = imageops::rotate90(image),
        2 => imageops::rotate180_in_place(image),
        3 => *image = imageops::rotate270(image),
        _ => {}
    }
    if mirror {
        imageops::flip_horizontal_in_place(image);
    }
}

/// How far a background is from the music's colour: the hue distance to its nearest strong
/// hue (a later, weaker hue counts a little less, a near-grey one hardly at all) and how
/// differently vivid that hue is, plus the difference in lightness.
pub(crate) fn distance(background: &Background, music: &CoverMusic, rule: &BackgroundRule) -> f64 {
    let hues = background.hues();
    let hue = if hues.is_empty() {
        1.0
    } else {
        hues.iter()
            .enumerate()
            .map(|(i, h)| {
                cover_colours::hue_distance(h.h, music.hue as f64) / 180.0
                    + i as f64 * rule.hue_step
                    + if h.c < rule.grey_below {
                        rule.grey_penalty
                    } else {
                        0.0
                    }
                    + (h.c - music.chroma).abs() * rule.chroma_weight
            })
            .fold(f64::INFINITY, f64::min)
    };
    hue + (background.mean_lightness - music.lightness).abs() * rule.lightness_weight
}

static CACHE: LazyLock<Mutex<HashMap<(String, i32), Arc<RgbImage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A background at `side` pixels, a copy the caller owns: the stored file halved, each pixel
/// (a + b + c + d + 2) / 4, while that still leaves at least the size wanted (so 600 is exactly
/// the reference's), then each pixel the mean of the area it covers, as the apps sample it.
pub fn load(book: &CoverBook, index: usize, side: i32) -> anyhow::Result<RgbImage> {
    let file = &book
        .backgrounds
        .get(index)
        .ok_or_else(|| anyhow!("no background {index}"))?
        .file;
    let key = (file.clone(), side);
    {
        let mut cache = CACHE.lock();
        if cache.len() > 12 {
            cache.clear();
        }
        if let Some(image) = cache.get(&key) {
            return Ok((**image).clone());
        }
    }
    let image = Arc::new(decode(file, side)?);
    let image = CACHE.lock().entry(key).or_insert(image).clone();
    Ok((*image).clone())
}

type Rgb = [u8; 3];

fn decode(file: &str, side: i32) -> anyhow::Result<RgbImage> {
    let bytes =
        design::background(file).ok_or_else(|| anyhow!("missing embedded cover design resource {file}"))?;
    let stored = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
        .with_context(|| format!("the background {file} does not decode"))?
        .to_rgb8();
    let mut size = stored.width() as usize;
    let side = side.max(1) as usize;
    let mut pixels: Vec<Rgb> = stored.pixels().map(|p| p.0).collect();
    while side * 2 <= size && size.is_multiple_of(2) {
        pixels = halve(&pixels, size);
        size /= 2;
    }
    if size != side {
        pixels = area_average(&pixels, size, side);
    }
    let raw: Vec<u8> = pixels.into_iter().flatten().collect();
    RgbImage::from_raw(side as u32, side as u32, raw)
        .ok_or_else(|| anyhow!("the background {file} is not square"))
}

fn halve(source: &[Rgb], size: usize) -> Vec<Rgb> {
    let half = size / 2;
    let mut output = vec![[0u8; 3]; half * half];
    for y in 0..half {
        for x in 0..half {
            let a = source[2 * y * size + 2 * x];
            let b = source[2 * y * size + 2 * x + 1];
            let c = source[(2 * y + 1) * size + 2 * x];
            let d = source[(2 * y + 1) * size + 2 * x + 1];
            let mean = |k: usize| ((a[k] as u32 + b[k] as u32 + c[k] as u32 + d[k] as u32 + 2) / 4) as u8;
            output[y * half + x] = [mean(0), mean(1), mean(2)];
        }
    }
    output
}

/// Each output pixel the mean of the source area it covers, part pixels in proportion, across then down.
fn area_average(source: &[Rgb], from: usize, side: usize) -> Vec<Rgb> {
    let scale = from as f64 / side as f64;
    let spans: Vec<(usize, Vec<f64>)> = (0..side)
        .map(|o| {
            let a = o as f64 * scale;
            let b = (o + 1) as f64 * scale;
            let start = (a.floor() as i64).clamp(0, from as i64 - 1) as usize;
            let end = (from as i64).min(b.ceil() as i64).max(start as i64 + 1) as usize;
            let weights = (0..end - start)
                .map(|k| {
                    let lo = a.max((start + k) as f64);
                    let hi = b.min((start + k) as f64 + 1.0);
                    (hi - lo).max(0.0) / (b - a)
                })
                .collect();
            (start, weights)
        })
        .collect();
    let mut across = vec![[0f64; 3]; side * from];
    for y in 0..from {
        for x in 0..side {
            let (start, weights) = &spans[x];
            let mut sum = [0f64; 3];
            for (k, w) in weights.iter().enumerate() {
                let p = source[y * from + start + k];
                sum[0] += p[0] as f64 * w;
                sum[1] += p[1] as f64 * w;
                sum[2] += p[2] as f64 * w;
            }
            across[y * side + x] = sum;
        }
    }
    let mut output = vec![[0u8; 3]; side * side];
    for (i, out) in output.iter_mut().enumerate() {
        let x = i % side;
        let (start, weights) = &spans[i / side];
        let channel = |c: usize| {
            let mut v = 0.0;
            for (k, w) in weights.iter().enumerate() {
                v += across[(start + k) * side + x][c] * w;
            }
            cover_colours::round(v).clamp(0, 255) as u8
        };
        *out = [channel(0), channel(1), channel(2)];
    }
    output
}
