//! The colour-keeping veil under a cover's words, exactly as cover-design.json "veil" says and
//! tools/cover-art/reference.py does: only what is too bright for white words is darkened, by
//! OKLab lightness with hue and chroma kept (yellows turned toward amber or green, never olive),
//! and only as far as each block of words asks.

use std::sync::LazyLock;

use image::RgbImage;

use super::cover_book::{CoverBook, VeilNumbers, VeilYellow};
use super::cover_layout::{CoverAlign, CoverWords, WordsRole};

/// One block of words' region: its box in pixels, how far its reach eases out, and its rule.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub falloff: [f64; 2],
    pub aim: f64,
    pub least: f64,
    pub max_drop: f64,
}

const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

type Matrix = [[f64; 3]; 3];

const M1: Matrix = [
    [0.4122214708, 0.5363325363, 0.0514459929],
    [0.2119034982, 0.6806995451, 0.1073969566],
    [0.0883024619, 0.2817188376, 0.6299787005],
];

const M2: Matrix = [
    [0.2104542553, 0.7936177850, -0.0040720468],
    [1.9779984951, -2.4285922050, 0.4505937099],
    [0.0259040371, 0.7827717662, -0.8086757660],
];

// The inverses are worked out from the forward matrices, as the reference does.
static M1_INVERSE: LazyLock<Matrix> = LazyLock::new(|| invert(&M1));
static M2_INVERSE: LazyLock<Matrix> = LazyLock::new(|| invert(&M2));

fn falloff(values: &[f64]) -> [f64; 2] {
    [
        values.first().copied().unwrap_or(1.0),
        values.get(1).copied().unwrap_or(1.0),
    ]
}

/// The regions for laid-out words: the name with its light line, and the foot line.
pub fn regions(book: &CoverBook, words: &[CoverWords], side: i32) -> Vec<Region> {
    let veil = &book.veil;
    let s = side as f64;
    let mut regions = Vec::new();
    let block: Vec<&CoverWords> = words
        .iter()
        .filter(|w| matches!(w.role, WordsRole::Title | WordsRole::Line))
        .collect();
    if let Some(first) = block.first() {
        let v = &veil.title;
        let pad = v.pad * s;
        let bottom = block
            .iter()
            .map(|w| w.top as f64 + w.measured.height as f64)
            .fold(f64::NEG_INFINITY, f64::max)
            + pad;
        let (x0, x1) = if first.align == CoverAlign::Right {
            (
                block
                    .iter()
                    .map(|w| w.inked()[0] as f64)
                    .fold(f64::INFINITY, f64::min)
                    - pad,
                s,
            )
        } else {
            (
                0.0,
                block
                    .iter()
                    .map(|w| w.inked()[2] as f64)
                    .fold(f64::NEG_INFINITY, f64::max)
                    + pad,
            )
        };
        regions.push(Region {
            x0,
            y0: 0.0,
            x1,
            y1: bottom,
            falloff: falloff(&v.falloff),
            aim: limit_for(v.aim_contrast, 1.0) * (1.0 - veil.margin),
            least: limit_for(v.min_contrast, 1.0) * (1.0 - veil.margin),
            max_drop: v.max_drop,
        });
    }
    if let Some(foot) = words.iter().find(|w| w.role == WordsRole::Footer) {
        let v = &veil.footer;
        let pad = v.pad * s;
        let inked = foot.inked();
        let (x0, x1) = if foot.align == CoverAlign::Right {
            (inked[0] as f64 - pad, s)
        } else {
            (0.0, inked[2] as f64 + pad)
        };
        let need = limit_for(v.contrast, book.layout.footer.opacity as f64) * (1.0 - veil.margin);
        regions.push(Region {
            x0,
            y0: foot.top as f64 - pad,
            x1,
            y1: s,
            falloff: falloff(&v.falloff),
            aim: need,
            least: need,
            max_drop: 1.0,
        });
    }
    regions
}

/// The most a background's luminance may be for white words at `opacity`, laid over it in sRGB
/// as the apps draw, to reach `contrast` on grey.
pub(crate) fn limit_for(contrast: f64, opacity: f64) -> f64 {
    if opacity >= 1.0 {
        return 1.05 / contrast - 0.05;
    }
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..40 {
        let mid = (low + high) / 2.0;
        let background = to_srgb(mid);
        let ink = to_linear(background + (1.0 - background) * opacity);
        if (ink + 0.05) / (mid + 0.05) >= contrast {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// Each 8-bit level in linear light, worked out once.
static LEVELS: LazyLock<[f64; 256]> = LazyLock::new(|| std::array::from_fn(|v| to_linear(v as f64 / 255.0)));

/// Darkens the background under the regions in place.
pub fn apply(image: &mut RgbImage, regions: &[Region], veil: &VeilNumbers) {
    if regions.is_empty() {
        return;
    }
    let side = image.width() as usize;
    veil_pixels(image.as_mut(), side, regions, veil);
}

/// Rows split into bands, one a thread, as C#'s Parallel.For spread them.
fn bands(side: usize) -> usize {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8);
    side.div_ceil(threads).max(1)
}

fn veil_pixels(pixels: &mut [u8], side: usize, regions: &[Region], veil: &VeilNumbers) {
    // Each region's reach at every pixel centre, across and down.
    let weights: Vec<(Vec<f64>, Vec<f64>)> = regions.iter().map(|r| weights(r, side)).collect();
    let yellow = &veil.yellow;
    let levels = &*LEVELS;
    let band = bands(side);
    let mut keeps = vec![1f32; side * side];

    // First the share of its luminance each pixel keeps, and which way the yellows lean.
    let mut row_sums = vec![(0f64, 0f64); side];
    std::thread::scope(|scope| {
        let chunks = pixels
            .chunks(band * side * 3)
            .zip(keeps.chunks_mut(band * side))
            .zip(row_sums.chunks_mut(band));
        for (index, ((rows, keeps), sums)) in chunks.enumerate() {
            let first = index * band;
            let weights = &weights;
            scope.spawn(move || {
                for (r, sum) in sums.iter_mut().enumerate() {
                    let row = first + r;
                    let (mut row_mass, mut row_hue) = (0.0, 0.0);
                    for col in 0..side {
                        let i = r * side + col;
                        let p = &rows[i * 3..i * 3 + 3];
                        let (rl, gl, bl) = (
                            levels[p[0] as usize],
                            levels[p[1] as usize],
                            levels[p[2] as usize],
                        );
                        let lum = LUMA[0] * rl + LUMA[1] * gl + LUMA[2] * bl;
                        // Each region keeps a share of the luminance; shares multiply, so the veil
                        // stays smooth where two regions meet.
                        let (mut keep, mut most) = (1.0f64, 0.0f64);
                        for (k, region) in regions.iter().enumerate() {
                            let across = weights[k].0[col];
                            let down = weights[k].1[row];
                            if across >= 1.0 || down >= 1.0 {
                                continue;
                            }
                            let w = 1.0 - smooth(0.0, 1.0, (across * across + down * down).sqrt());
                            if w <= 0.0 {
                                continue;
                            }
                            let limit = region.aim.max(lum * (1.0 - region.max_drop)).min(region.least);
                            keep *= 1.0 - w * (1.0 - (limit / lum.max(1e-9)).min(1.0));
                            most = most.max(w);
                        }
                        keeps[i] = keep as f32;
                        if most <= 0.0 {
                            continue;
                        }
                        let (_, a, b) = to_oklab(rl, gl, bl);
                        let (h, c, wy) = yellowness(a, b, yellow);
                        if wy <= 0.0 {
                            continue;
                        }
                        let mass = wy * c * most;
                        row_mass += mass;
                        row_hue += h * mass;
                    }
                    *sum = (row_mass, row_hue);
                }
            });
        }
    });
    let (mass_sum, hue_sum) = row_sums
        .iter()
        .fold((0.0, 0.0), |(m, h), (rm, rh)| (m + rm, h + rh));

    // One direction per cover: yellows under the veil that lean yellow deepen to amber,
    // those that lean lime to green.
    let lean = if mass_sum > 0.0 { hue_sum / mass_sum } else { 0.0 };
    let towards = if lean <= yellow.split {
        yellow.towards[0]
    } else {
        yellow.towards[1]
    };

    std::thread::scope(|scope| {
        for (rows, keeps) in pixels.chunks_mut(band * side * 3).zip(keeps.chunks(band * side)) {
            scope.spawn(move || {
                for (i, &keep) in keeps.iter().enumerate() {
                    if keep >= 1.0 {
                        continue;
                    }
                    let p = &mut rows[i * 3..i * 3 + 3];
                    let (rl, gl, bl) = (
                        levels[p[0] as usize],
                        levels[p[1] as usize],
                        levels[p[2] as usize],
                    );
                    let y0 = LUMA[0] * rl + LUMA[1] * gl + LUMA[2] * bl;
                    let t = y0 * keep as f64;
                    if t >= y0 - 1e-9 {
                        continue;
                    }
                    let (l, a, b) = to_oklab(rl, gl, bl);
                    let drop = 1.0 - t / y0;
                    let (h, c, wy) = yellowness(a, b, yellow);
                    let share = (yellow.turn_per_drop * drop).clamp(0.0, 1.0) * wy;
                    let h2 = h + (towards - h) * share;
                    let c2 = c * (1.0 + yellow.chroma_lift * drop * wy);
                    let a2 = c2 * (h2 * std::f64::consts::PI / 180.0).cos();
                    let b2 = c2 * (h2 * std::f64::consts::PI / 180.0).sin();

                    // Lightness for the target luminance: exact for greys at first, then refined
                    // against what the colour settles to after gamut mapping.
                    let mut l2 = l * (t / y0).cbrt();
                    for _ in 0..veil.refine {
                        let (sr, sg, sb) = settle(l2, a2, b2);
                        let got = LUMA[0] * sr + LUMA[1] * sg + LUMA[2] * sb;
                        l2 *= (t / got.max(1e-6)).cbrt();
                    }
                    let (fr, fg, fb) = settle(l2, a2, b2);
                    p[0] = byte(fr);
                    p[1] = byte(fg);
                    p[2] = byte(fb);
                }
            });
        }
    });
}

fn weights(r: &Region, side: usize) -> (Vec<f64>, Vec<f64>) {
    let mut across = vec![0.0; side];
    let mut down = vec![0.0; side];
    for i in 0..side {
        let c = i as f64 + 0.5;
        across[i] = (r.x0 - c).max(c - r.x1).max(0.0) / (r.falloff[0] * side as f64);
        down[i] = (r.y0 - c).max(c - r.y1).max(0.0) / (r.falloff[1] * side as f64);
    }
    (across, down)
}

fn yellowness(a: f64, b: f64, yellow: &VeilYellow) -> (f64, f64, f64) {
    let c = (a * a + b * b).sqrt();
    let mut h = b.atan2(a) * 180.0 / std::f64::consts::PI;
    h = (h % 360.0 + 360.0) % 360.0;
    let wy = smooth(yellow.hues[0], yellow.hues[1], h)
        * (1.0 - smooth(yellow.hues[2], yellow.hues[3], h))
        * (c / yellow.chroma_from).clamp(0.0, 1.0);
    (h, c, wy)
}

/// OKLab to linear sRGB inside the gamut: chroma eased toward grey where it is out, clipped
/// where it is only just out.
fn settle(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let raw = to_linear_rgb(l, a, b);
    let low = raw.0.min(raw.1.min(raw.2));
    let high = raw.0.max(raw.1.max(raw.2));
    if low >= 0.0 && high <= 1.0 {
        return raw;
    }

    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        let test = to_linear_rgb(l, a * mid, b * mid);
        let ok = test.0.min(test.1.min(test.2)) >= -1e-4 && test.0.max(test.1.max(test.2)) <= 1.0 + 1e-4;
        if ok {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let reduced = to_linear_rgb(l, a * lo, b * lo);
    let excess = (-low).max(high - 1.0);
    let w = smooth(0.0, 1.0, (excess / 0.05).clamp(0.0, 1.0));
    let mix = |r: f64, d: f64| r.clamp(0.0, 1.0) * (1.0 - w) + d.clamp(0.0, 1.0) * w;
    (
        mix(raw.0, reduced.0).clamp(0.0, 1.0),
        mix(raw.1, reduced.1).clamp(0.0, 1.0),
        mix(raw.2, reduced.2).clamp(0.0, 1.0),
    )
}

fn to_oklab(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let l = (M1[0][0] * r + M1[0][1] * g + M1[0][2] * b).cbrt();
    let m = (M1[1][0] * r + M1[1][1] * g + M1[1][2] * b).cbrt();
    let s = (M1[2][0] * r + M1[2][1] * g + M1[2][2] * b).cbrt();
    (
        M2[0][0] * l + M2[0][1] * m + M2[0][2] * s,
        M2[1][0] * l + M2[1][1] * m + M2[1][2] * s,
        M2[2][0] * l + M2[2][1] * m + M2[2][2] * s,
    )
}

fn to_linear_rgb(big_l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let m2 = &*M2_INVERSE;
    let m1 = &*M1_INVERSE;
    let l = m2[0][0] * big_l + m2[0][1] * a + m2[0][2] * b;
    let m = m2[1][0] * big_l + m2[1][1] * a + m2[1][2] * b;
    let s = m2[2][0] * big_l + m2[2][1] * a + m2[2][2] * b;
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    (
        m1[0][0] * l + m1[0][1] * m + m1[0][2] * s,
        m1[1][0] * l + m1[1][1] * m + m1[1][2] * s,
        m1[2][0] * l + m1[2][1] * m + m1[2][2] * s,
    )
}

pub(crate) fn to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn to_srgb(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Linear light to an 8-bit level, rounding half to even as the reference does.
fn byte(linear: f64) -> u8 {
    (to_srgb(linear) * 255.0).round_ties_even().clamp(0.0, 255.0) as u8
}

fn smooth(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn invert(m: &Matrix) -> Matrix {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    [
        [
            (e * i - f * h) / det,
            (c * h - b * i) / det,
            (b * f - c * e) / det,
        ],
        [
            (f * g - d * i) / det,
            (a * i - c * g) / det,
            (c * d - a * f) / det,
        ],
        [
            (d * h - e * g) / det,
            (b * g - a * h) / det,
            (a * e - b * d) / det,
        ],
    ]
}
