//! The colour rules of the cover design, the same maths as the Octo apps' covers
//! (app.winters.octo.covers) so a list looks the same wherever it is drawn. Colours
//! are 0xAARRGGBB ints; rounding is half up, as the apps round.

use std::collections::HashMap;

/// A colour as OKLCH: lightness 0 to 1, chroma (0 grey, about 0.3 at the most vivid), hue in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

/// One colour of a picture and how much of it that colour covers, 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Swatch {
    pub argb: i32,
    pub share: f32,
}

pub const WHITE: i32 = 0xFFFF_FFFF_u32 as i32;
pub const BLACK: i32 = 0xFF00_0000_u32 as i32;

/// Half up, as the apps round.
pub(crate) fn round(v: f64) -> i32 {
    (v + 0.5).floor() as i32
}

/// Half up, in single precision, as the C# `Round(float)` overload.
pub(crate) fn round_f32(v: f32) -> i32 {
    (v + 0.5f32).floor() as i32
}

/// `#rrggbb` (the `#` optional) as an opaque colour.
pub fn hex(hex: &str) -> Result<i32, std::num::ParseIntError> {
    let value = u32::from_str_radix(hex.trim_start_matches('#'), 16)?;
    Ok((0xFF00_0000 | value) as i32)
}

pub fn r(argb: i32) -> i32 {
    (argb >> 16) & 0xFF
}

pub fn g(argb: i32) -> i32 {
    (argb >> 8) & 0xFF
}

pub fn b(argb: i32) -> i32 {
    argb & 0xFF
}

pub fn a(argb: i32) -> i32 {
    ((argb as u32) >> 24) as i32 & 0xFF
}

fn linear(channel: i32) -> f64 {
    let c = channel as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn to_lch(argb: i32) -> Lch {
    let r = linear(r(argb));
    let g = linear(g(argb));
    let b = linear(b(argb));
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    let lightness = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s;
    let a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
    let bb = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
    let mut hue = bb.atan2(a) * 180.0 / std::f64::consts::PI;
    if hue < 0.0 {
        hue += 360.0;
    }
    Lch {
        l: lightness,
        c: (a * a + bb * bb).sqrt(),
        h: hue,
    }
}

/// How far apart two hues are around the circle, 0 to 180 degrees.
pub fn hue_distance(a: f64, b: f64) -> f64 {
    let d = ((a - b) % 360.0 + 360.0) % 360.0;
    if d > 180.0 { 360.0 - d } else { d }
}

/// How far apart two colours look (0 the same).
pub fn distance(x: Lch, y: Lch) -> f64 {
    let ra = x.h * std::f64::consts::PI / 180.0;
    let rb = y.h * std::f64::consts::PI / 180.0;
    let da = x.c * ra.cos() - y.c * rb.cos();
    let db = x.c * ra.sin() - y.c * rb.sin();
    ((x.l - y.l).powi(2) + da * da + db * db).sqrt()
}

/// The colour with this opacity.
pub fn alpha(argb: i32, a: f32) -> i32 {
    (round_f32(a.clamp(0.0, 1.0) * 255.0) << 24) | (argb & 0xFF_FFFF)
}

/// One colour laid over another at the first one's opacity.
pub fn over(top: i32, under: i32) -> i32 {
    let a = self::a(top) as f32 / 255.0;
    if a >= 1.0 {
        return top | BLACK;
    }
    let channel = |shift: i32| {
        let t = (top >> shift) & 0xFF;
        let u = (under >> shift) & 0xFF;
        round_f32(u as f32 + (t - u) as f32 * a).clamp(0, 255)
    };
    BLACK | (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// WCAG relative luminance, with the apps' 0.03928 knee.
pub fn relative_luminance(argb: i32) -> f64 {
    fn lin(channel: i32) -> f64 {
        let c = channel as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(r(argb)) + 0.7152 * lin(g(argb)) + 0.0722 * lin(b(argb))
}

/// How far apart two colours are for reading, from 1 (the same) to 21.
pub fn contrast_ratio(a: i32, b: i32) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// A number from some text, the same on every device (FNV-1a 64 over its UTF-8), never negative.
pub fn cover_hash(text: &str) -> i64 {
    (fnv1a(text) >> 1) as i64
}

/// The number that picks a list's background and its turn, the same on every device: FNV-1a
/// 64 over the id's UTF-8, then MurmurHash3's fmix64 so ids that differ only in their last
/// letter still land far apart, then shifted right once (never negative).
pub fn cover_pick(text: &str) -> i64 {
    let mut k = fnv1a(text);
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    (k >> 1) as i64
}

#[derive(Clone)]
struct Bucket {
    key: i32,
    count: i32,
    r: i64,
    g: i64,
    b: i64,
}

impl Bucket {
    fn mean(&self) -> i32 {
        let count = i64::from(self.count);
        BLACK | (((self.r / count) as i32) << 16) | (((self.g / count) as i32) << 8) | (self.b / count) as i32
    }
}

/// A picture's main colours, most of the picture first, up to `most`: pixels counted in coarse
/// buckets (4 bits a channel), every `step`th one, each bucket joining the first colour it looks
/// like or starting one of its own. (C# defaults: step 3, most 6.)
pub fn swatches(pixels: &[i32], step: usize, most: usize) -> Vec<Swatch> {
    if pixels.is_empty() {
        return Vec::new();
    }
    let mut counts: HashMap<i32, Bucket> = HashMap::new();
    for &p in pixels.iter().step_by(step.max(1)) {
        let key = (((p >> 20) & 0xF) << 8) | (((p >> 12) & 0xF) << 4) | ((p >> 4) & 0xF);
        let bucket = counts.entry(key).or_insert(Bucket {
            key,
            count: 0,
            r: 0,
            g: 0,
            b: 0,
        });
        bucket.count += 1;
        bucket.r += i64::from(r(p));
        bucket.g += i64::from(g(p));
        bucket.b += i64::from(b(p));
    }
    let mut found: Vec<Bucket> = counts.into_values().collect();
    found.sort_by(|x, y| y.count.cmp(&x.count).then(x.key.cmp(&y.key)));
    let total = found.iter().map(|b| i64::from(b.count)).sum::<i64>() as f32;
    let mut groups: Vec<(Lch, Bucket)> = Vec::new();
    for bucket in &found {
        // Specks of a colour (under 0.2% of the picture) are left out.
        if (bucket.count as f32) < total * 0.002 {
            break;
        }
        let seen = to_lch(bucket.mean());
        if let Some((_, near)) = groups.iter_mut().find(|(g, _)| distance(*g, seen) < 0.09) {
            near.count += bucket.count;
            near.r += bucket.r;
            near.g += bucket.g;
            near.b += bucket.b;
        } else if groups.len() < most * 3 {
            groups.push((seen, bucket.clone()));
        }
    }
    let mut buckets: Vec<Bucket> = groups.into_iter().map(|(_, b)| b).collect();
    // A stable sort, as LINQ's OrderByDescending: equal counts keep the order they were found in.
    buckets.sort_by_key(|b| std::cmp::Reverse(b.count));
    buckets
        .into_iter()
        .take(most)
        .map(|b| Swatch {
            argb: b.mean(),
            share: b.count as f32 / total,
        })
        .collect()
}

/// The colour of a list's music that picks its cover's background: hue (whole degrees), chroma
/// and lightness (OKLCH, to 0.001) of the strongest colour of its first covers, rounded as the
/// Octo apps round it so both pick the same background.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverMusic {
    pub hue: i32,
    pub chroma: f64,
    pub lightness: f64,
}

impl CoverMusic {
    /// Colours under this chroma read as grey, and give no hue to work from.
    const COLOURLESS: f64 = 0.035;

    /// The colour as short text, for cache keys.
    pub fn key(&self) -> String {
        format!(
            "{}.{}.{}",
            self.hue,
            round(self.chroma * 1000.0),
            round(self.lightness * 1000.0)
        )
    }

    pub fn of(hue: f64, chroma: f64, lightness: f64) -> CoverMusic {
        CoverMusic {
            hue: (round(hue) % 360 + 360) % 360,
            chroma: round(chroma.clamp(0.0, 0.4) * 1000.0) as f64 / 1000.0,
            lightness: round(lightness.clamp(0.0, 1.0) * 1000.0) as f64 / 1000.0,
        }
    }

    /// The music's colour from the main colours of some covers (a list of swatches for each):
    /// the strongest by how much of its cover it fills and how vivid it is. None with no covers
    /// or only grey ones.
    pub fn from_covers(covers: &[Vec<Swatch>]) -> Option<CoverMusic> {
        let with_colour: Vec<&Vec<Swatch>> = covers.iter().filter(|c| !c.is_empty()).collect();
        if with_colour.is_empty() {
            return None;
        }
        let n = with_colour.len() as f64;
        let colourful: Vec<(Lch, f64)> = with_colour
            .iter()
            .flat_map(|swatches| {
                swatches
                    .iter()
                    .take(4)
                    .map(|s| (to_lch(s.argb), s.share as f64 / n))
            })
            .filter(|(lch, _)| lch.c >= Self::COLOURLESS && (0.15..=0.97).contains(&lch.l))
            .collect();
        // LINQ's MaxBy keeps the first of equal scores.
        let mut first: Option<(Lch, f64)> = None;
        for &(lch, weight) in &colourful {
            let score = weight.sqrt() * (0.3 + lch.c * 5.0);
            if first.is_none_or(|(_, best)| score > best) {
                first = Some((lch, score));
            }
        }
        first.map(|(lch, _)| CoverMusic::of(lch.h, lch.c, lch.l))
    }
}
