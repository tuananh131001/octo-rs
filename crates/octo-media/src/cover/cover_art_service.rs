//! Covers Octo draws itself: the small Octo badge on covers of songs found outside the library
//! (for third-party clients), the placeholder when no cover can be had, and the covers of the
//! lists Octo makes (radio stations and mixes), designed like the Octo apps' playlist covers:
//! a painted background picked to match the list's music, darkened only under the words, with
//! its name in white. A picture someone put in the covers folder replaces a list's cover.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use image::imageops::FilterType;
use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage};
use parking_lot::Mutex;
use tracing::{debug, error, info, warn};

use super::cover_backgrounds;
use super::cover_book::CoverBook;
use super::cover_colours::{self, CoverMusic, Swatch};
use super::cover_fonts::{self, CoverTypesetter};
use super::cover_image;
use super::cover_layout::{self, CoverSpec};
use super::cover_painter::{self, CoverArt};
use super::text::{eq_ignore_case, strip_suffix_ignore_case, utf16_len};

/// The kinds of list Octo makes.
pub mod list_kinds {
    pub const RADIO: &str = "radio";
    pub const MIX: &str = "mix";
}

/// A boxed future, as the seed callbacks return them.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// How a list hands over its seed songs' covers.
pub type SeedsFn = Arc<dyn Fn() -> BoxFuture<anyhow::Result<Vec<CoverSeed>>> + Send + Sync>;

/// How one seed cover is fetched.
pub type FetchFn = Arc<dyn Fn() -> BoxFuture<anyhow::Result<Option<Vec<u8>>>> + Send + Sync>;

/// A list whose cover Octo draws: its name, its genre or decade, its kind, its song count if
/// known, and the songs whose covers give it its colours.
#[derive(Clone)]
pub struct ListCover {
    pub name: String,
    pub label: Option<String>,
    pub kind: String,
    pub seeds: Option<SeedsFn>,
    pub song_count: Option<i32>,
}

impl ListCover {
    pub fn new(name: impl Into<String>) -> ListCover {
        ListCover {
            name: name.into(),
            label: None,
            kind: list_kinds::MIX.to_string(),
            seeds: None,
            song_count: None,
        }
    }
}

/// One seed cover: what it is (so its colours can be remembered by it) and how to get it.
#[derive(Clone)]
pub struct CoverSeed {
    pub identity: String,
    pub fetch: FetchFn,
}

/// List covers are drawn at the size asked, within these bounds.
pub(crate) const MIN_COVER_SIZE: i32 = 600;
pub(crate) const MAX_COVER_SIZE: i32 = 1200;

const SEED_WAIT: Duration = Duration::from_secs(4);
const MUSIC_HIT: Duration = Duration::from_secs(12 * 60 * 60);
const MUSIC_GREY: Duration = Duration::from_secs(10 * 60);
const MUSIC_MISS: Duration = Duration::from_secs(60);
/// The list covers' JPEG: quality 92, no chroma subsampling (ImageSharp's YCbCrRatio444).
const JPEG_QUALITY: u8 = 92;

/// The lightness a genre's or decade's stand-in colour is given: the middle of the library's.
pub(crate) const GENRE_LIGHTNESS: f64 = 0.62;

/// Draws list covers, the placeholder and the Octo badge.
pub struct CoverArtService {
    logo: OnceLock<Option<RgbaImage>>,
    logo_paths: Vec<PathBuf>,
    named_covers: Mutex<HashMap<String, Vec<u8>>>,
    music_memo: Mutex<HashMap<String, (Option<CoverMusic>, Instant)>>,
    covers_directory: Option<PathBuf>,
    book: &'static CoverBook,
    setter: CoverTypesetter,
}

impl CoverArtService {
    /// `covers_directory`: pictures that replace a generated cover, named after the playlist
    /// (/app/config/covers). The logo is looked for beside the executable, in `Assets/` or
    /// `wwwroot/Assets/`, as the C# app looked in its publish root.
    pub fn new(covers_directory: Option<PathBuf>) -> CoverArtService {
        let base = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        CoverArtService {
            logo: OnceLock::new(),
            logo_paths: vec![
                base.join("Assets").join("octo_logo.png"),
                base.join("wwwroot").join("Assets").join("octo_logo.png"),
            ],
            named_covers: Mutex::new(HashMap::new()),
            music_memo: Mutex::new(HashMap::new()),
            covers_directory: covers_directory.filter(|d| !d.as_os_str().to_string_lossy().trim().is_empty()),
            book: CoverBook::shipped(),
            setter: CoverTypesetter,
        }
    }

    /// The same service with its own design (the shipped one by default).
    pub fn with_book(mut self, book: &'static CoverBook) -> CoverArtService {
        self.book = book;
        self
    }

    /// The same service, looking for the logo at these paths, in order.
    pub fn with_logo_paths(mut self, paths: Vec<PathBuf>) -> CoverArtService {
        self.logo_paths = paths;
        self.logo = OnceLock::new();
        self
    }

    fn octo_logo(&self) -> Option<&RgbaImage> {
        self.logo
            .get_or_init(|| {
                for path in &self.logo_paths {
                    if !path.is_file() {
                        continue;
                    }
                    match image::open(path) {
                        Ok(logo) => {
                            let logo = logo.to_rgba8();
                            info!(
                                "Octo logo loaded from {} ({}x{})",
                                path.display(),
                                logo.width(),
                                logo.height()
                            );
                            return Some(logo);
                        }
                        Err(e) => warn!(error = %e, "Failed to load Octo logo from {}", path.display()),
                    }
                }
                let paths: Vec<String> = self.logo_paths.iter().map(|p| p.display().to_string()).collect();
                warn!(
                    "Octo logo not found at any of: {}; radio cover badges disabled",
                    paths.join(", ")
                );
                None
            })
            .as_ref()
    }

    fn sized_logo(&self, width: u32, height: u32) -> Option<RgbaImage> {
        let logo = self.octo_logo()?;
        // ImageSharp's default resampler is bicubic (Catmull-Rom).
        Some(image::imageops::resize(
            logo,
            width.max(1),
            height.max(1),
            FilterType::CatmullRom,
        ))
    }

    /// Composites the Octo logo onto the top left of an existing cover art image. Returns the
    /// modified bytes as JPEG, or the original bytes unchanged if the logo is missing or the
    /// source image fails to decode.
    pub fn add_octo_badge(&self, original_art: &[u8]) -> Vec<u8> {
        let Some(image) = cover_image::decode(original_art) else {
            error!("Failed to composite Octo badge onto cover art");
            return original_art.to_vec();
        };
        let mut image = image.to_rgba8();
        let image_size = image.width().min(image.height());
        // Logo footprint as a fraction of the cover. 28% reads clearly even at the 100-150px
        // thumbnails most clients use for queue rows.
        let badge_size = (image_size as f64 * 0.28) as u32;
        let padding = (image_size as f64 * 0.03) as i64;
        let Some(badge) = self.sized_logo(badge_size, badge_size) else {
            return original_art.to_vec();
        };
        // Top-left placement: most album covers concentrate visual content and text along the
        // center/bottom (artist name, track titles, overlay UI from clients), so top-left is
        // consistently the "quietest" region. Also matches Western reading-order so it's the
        // first thing the eye picks up — exactly what a source indicator wants.
        image::imageops::overlay(&mut image, &badge, padding, padding);
        cover_image::encode_jpeg(&DynamicImage::ImageRgba8(image), 90)
            .unwrap_or_else(|| original_art.to_vec())
    }

    /// Returns a 600x600 placeholder JPEG with the Octo logo centered on a black background.
    /// Used when iTunes lookup whiffs so we never 404 a cover-art request — Subsonic clients
    /// drop entries whose cover fetch fails.
    ///
    /// `branded`: whether to stamp the Octo logo. True for external tracks, where the badge
    /// says where the track came from. **False for anything in the user's own library**: a
    /// local file that simply has no embedded art, or whose art could not be read in time, is
    /// not Octo's, and branding it reads as Octo claiming a song the user already owned.
    pub fn get_placeholder_cover(&self, branded: bool) -> Vec<u8> {
        const SIZE: u32 = 600;
        let mut image: RgbaImage = ImageBuffer::from_pixel(SIZE, SIZE, Rgba([0, 0, 0, 255]));
        if branded {
            let logo_size = (SIZE as f64 * 0.55) as u32;
            if let Some(sized) = self.sized_logo(logo_size, logo_size) {
                let at = i64::from((SIZE - logo_size) / 2);
                image::imageops::overlay(&mut image, &sized, at, at);
            }
        }
        if let Some(bytes) = cover_image::encode_jpeg(&DynamicImage::ImageRgba8(image), 85) {
            return bytes;
        }
        error!("Failed to render Octo placeholder cover");
        // Last-ditch: a tiny solid-black JPEG so we still respond 200.
        let fallback: RgbaImage = ImageBuffer::from_pixel(64, 64, Rgba([0, 0, 0, 255]));
        cover_image::encode_jpeg(&DynamicImage::ImageRgba8(fallback), 70).unwrap_or_default()
    }

    /// Draws one cover and forgets it, so the fonts are loaded before anyone asks. Blocking:
    /// call it from a background task (`spawn_blocking`).
    pub fn warm(&self) {
        let warmed = self.render(
            &Self::spec("Warm Radio", Some(list_kinds::RADIO), Some(1), None),
            MIN_COVER_SIZE,
            true,
        );
        let _ = cover_fonts::fallbacks().len();
        if let Err(e) = warmed {
            warn!(error = %e, "The cover fonts could not be loaded; list covers will be plain placeholders");
        }
    }

    /// A radio station's cover from its name alone. It's a playlist like any other, with no Octo mark.
    pub fn get_radio_station_cover(&self, station_name: &str, size: Option<i32>) -> Vec<u8> {
        let name = if station_name.trim().is_empty() {
            "Octo Radio"
        } else {
            station_name.trim()
        };
        self.get_named_cover(name, None, size, list_kinds::RADIO)
    }

    /// A list's cover from its name alone: its genre's or decade's colour, or its design's own.
    pub fn get_named_cover(&self, name: &str, label: Option<&str>, size: Option<i32>, kind: &str) -> Vec<u8> {
        let list = ListCover {
            name: name.to_string(),
            label: label.map(str::to_string),
            kind: kind.to_string(),
            seeds: None,
            song_count: None,
        };
        let (size, shown, lookup) = Self::names(&list, size);
        if let Some(custom) = self.override_cover(&shown, &lookup, size) {
            return custom;
        }
        self.design_cover(&list, &shown, &lookup, size, None)
    }

    /// A list's cover. In order: a picture in the covers folder named after the list or its
    /// genre or decade; a painted background matched to the colours of its seed songs' covers,
    /// else to its genre's or decade's colour, else picked by its name; and last a plain
    /// placeholder, never the logo. A replaced picture or a new seed cover shows without a
    /// restart. Dropping the future cancels it, as C#'s token did.
    pub async fn get_list_cover(&self, list: &ListCover, requested_size: Option<i32>) -> Vec<u8> {
        let (size, shown, lookup) = Self::names(list, requested_size);
        if let Some(custom) = self.override_cover(&shown, &lookup, size) {
            return custom;
        }
        match self.seed_music(list, &shown).await {
            Ok(seeded) => self.design_cover(list, &shown, &lookup, size, seeded),
            Err(e) => {
                warn!(error = %e, "Could not draw a cover for {}", shown);
                // Nothing could be drawn: a plain placeholder, never the logo.
                self.get_placeholder_cover(false)
            }
        }
    }

    /// The size to draw, the name to show, and the name to look a picture or hue up by.
    fn names(list: &ListCover, requested_size: Option<i32>) -> (i32, String, String) {
        let size = Self::cover_size(requested_size);
        let shown = if list.name.trim().is_empty() {
            "Octo".to_string()
        } else {
            list.name.trim().to_string()
        };
        let lookup = match list.label.as_deref() {
            Some(label) if !label.trim().is_empty() => label.trim().to_string(),
            _ => shown.clone(),
        };
        (size, shown, lookup)
    }

    /// The covers folder's picture for the list, sized, when there is one that reads.
    fn override_cover(&self, shown: &str, lookup: &str, size: i32) -> Option<Vec<u8>> {
        let custom = self.find_override(shown, lookup)?;
        let ticks = std::fs::metadata(&custom)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let override_key = format!("override\n{}\n{ticks}\n{size}", custom.display());
        if let Some(cached) = self.named_covers.lock().get(&override_key) {
            return Some(cached.clone());
        }
        let picture = self.load_override(&custom, size)?;
        Some(self.remember(override_key, picture))
    }

    /// The designed cover: remembered, or drawn now.
    fn design_cover(
        &self,
        list: &ListCover,
        shown: &str,
        lookup: &str,
        size: i32,
        seeded: Option<CoverMusic>,
    ) -> Vec<u8> {
        let music = seeded.or_else(|| self.fallback_music(shown, lookup));
        let spec = Self::spec(shown, Some(&list.kind), list.song_count, music);
        let background = cover_backgrounds::choose(self.book, spec.music.as_ref(), &spec.id);
        let key = format!(
            "{}\n{}\n{}\n{}\n{}\n{background}\n{size}",
            self.book.version,
            spec.id,
            spec.name,
            spec.line.as_deref().unwrap_or(""),
            spec.footer.as_deref().unwrap_or("")
        );
        if let Some(drawn) = self.named_covers.lock().get(&key) {
            return drawn.clone();
        }
        match self.render(&spec, size, true) {
            Ok(bytes) => self.remember(key, bytes),
            Err(e) => {
                warn!(error = %e, "Could not draw a cover for {}", shown);
                // Nothing could be drawn: a plain placeholder, never the logo. A playlist's
                // cover is its own, whether Octo made the list or the listener did.
                self.get_placeholder_cover(false)
            }
        }
    }

    fn remember(&self, key: String, bytes: Vec<u8>) -> Vec<u8> {
        let mut covers = self.named_covers.lock();
        if covers.len() >= 256 {
            covers.clear();
        }
        covers.entry(key).or_insert(bytes).clone()
    }

    pub(crate) fn cover_size(requested: Option<i32>) -> i32 {
        requested
            .filter(|r| *r > 0)
            .unwrap_or(MIN_COVER_SIZE)
            .clamp(MIN_COVER_SIZE, MAX_COVER_SIZE)
    }

    /// A stand-in for the music when the songs give none: the genre's or decade's hue, else nothing.
    pub(crate) fn fallback_music(&self, shown: &str, lookup: &str) -> Option<CoverMusic> {
        let (hue, chroma) = self
            .book
            .list_hue(Some(lookup))
            .or_else(|| self.book.list_hue(Some(shown)))?;
        Some(CoverMusic::of(hue, chroma, GENRE_LIGHTNESS))
    }

    /// What goes on a list's cover. The name is the list's own, less a trailing "Radio" on a
    /// station or "Mix" on a mix, since the light line under it says which it is; "Your Mix"
    /// stays whole. A name that still ends in a word saying what it is ("Your Mix", "Discovery
    /// Mix") has no light line, as the name already says it. The foot line is its song count
    /// when known. The design is picked by the list's full name, the same on every request.
    pub(crate) fn spec(
        shown: &str,
        kind: Option<&str>,
        song_count: Option<i32>,
        music: Option<CoverMusic>,
    ) -> CoverSpec {
        let (line, suffix) = if kind == Some(list_kinds::RADIO) {
            ("Station", " Radio")
        } else {
            ("Mix", " Mix")
        };
        let mut title = shown;
        if let Some(head) = strip_suffix_ignore_case(shown, suffix) {
            let head = head.trim();
            if utf16_len(head) > 1 && !POSSESSIVES.iter().any(|p| eq_ignore_case(p, head)) {
                title = head;
            }
        }
        CoverSpec {
            id: shown.to_string(),
            name: title.to_string(),
            line: if cover_layout::says_what_it_is(title) {
                None
            } else {
                Some(line.to_string())
            },
            footer: Self::footer(song_count),
            music,
        }
    }

    pub(crate) fn footer(songs: Option<i32>) -> Option<String> {
        match songs {
            Some(1) => Some("1 song".to_string()),
            Some(n) if n > 1 => Some(format!("{} songs", thousands(n))),
            _ => None,
        }
    }

    /// The cover as the server serves it: painted, then a JPEG.
    pub(crate) fn render(&self, spec: &CoverSpec, size: i32, draw_words: bool) -> anyhow::Result<Vec<u8>> {
        let image = self.paint(spec, size, draw_words)?;
        cover_image::encode_jpeg(&DynamicImage::ImageRgb8(image), JPEG_QUALITY)
            .ok_or_else(|| anyhow::anyhow!("the cover does not encode"))
    }

    pub(crate) fn paint(
        &self,
        spec: &CoverSpec,
        size: i32,
        draw_words: bool,
    ) -> anyhow::Result<image::RgbImage> {
        cover_painter::paint(self.book, &self.compose(spec, size), &self.setter, draw_words)
    }

    /// The cover's background, how it is turned, and where its words go.
    pub(crate) fn compose(&self, spec: &CoverSpec, size: i32) -> CoverArt {
        CoverArt {
            side: size,
            background: cover_backgrounds::choose(self.book, spec.music.as_ref(), &spec.id),
            orientation: cover_backgrounds::orientation(self.book, &spec.id),
            words: cover_layout::words(spec, size, &self.setter, self.book),
        }
    }

    /// Colours from the list's seed covers: fetched until two pictures are in hand, all within
    /// a few seconds. Remembered by which seeds they were, so a new seed can change the cover
    /// and an unchanged one costs nothing. None when there are none to read. An error only when
    /// the list could not say what its seeds are.
    async fn seed_music(&self, list: &ListCover, shown: &str) -> anyhow::Result<Option<CoverMusic>> {
        let Some(seeds_fn) = list.seeds.clone() else {
            return Ok(None);
        };
        let work = async {
            let seeds = seeds_fn().await?;
            if seeds.is_empty() {
                return Ok(None);
            }
            let memo_key = std::iter::once(shown)
                .chain(seeds.iter().map(|seed| seed.identity.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if let Some((music, until)) = self.music_memo.lock().get(&memo_key)
                && *until > Instant::now()
            {
                return Ok(*music);
            }

            let mut covers: Vec<Vec<Swatch>> = Vec::new();
            for seed in &seeds {
                if covers.len() >= 2 {
                    break;
                }
                match (seed.fetch)().await {
                    Ok(Some(bytes)) if !bytes.is_empty() => {
                        if let Some(swatches) = Self::swatches_of(&bytes) {
                            covers.push(swatches);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => debug!(error = %e, "A seed cover for {} could not be fetched", shown),
                }
            }
            let music = if covers.is_empty() {
                None
            } else {
                CoverMusic::from_covers(&covers)
            };
            let ttl = if music.is_some() {
                MUSIC_HIT
            } else if !covers.is_empty() {
                MUSIC_GREY
            } else {
                MUSIC_MISS
            };
            let mut memo = self.music_memo.lock();
            if memo.len() >= 1024 {
                memo.clear();
            }
            memo.insert(memo_key, (music, Instant::now() + ttl));
            Ok(music)
        };
        match tokio::time::timeout(SEED_WAIT, work).await {
            Ok(result) => result,
            // Out of time: the fallback colours now, the seeds on a later request.
            Err(_) => Ok(None),
        }
    }

    /// A picture's main colours, read from a small copy of it.
    pub(crate) fn swatches_of(picture: &[u8]) -> Option<Vec<Swatch>> {
        let image = cover_image::decode(picture)?;
        // C# asked ImageSharp's decoder for a 64 by 64 target, which shrinks the picture to fit
        // inside it, keeping its shape, with a box filter; the triangle filter is the nearest
        // the image crate has.
        let image = if image.width() > 64 || image.height() > 64 {
            image.resize(64, 64, FilterType::Triangle)
        } else {
            image
        };
        let rgba = image.to_rgba8();
        let pixels: Vec<i32> = rgba
            .pixels()
            .map(|p| {
                cover_colours::BLACK
                    | (i32::from(p.0[0]) << 16)
                    | (i32::from(p.0[1]) << 8)
                    | i32::from(p.0[2])
            })
            .collect();
        Some(cover_colours::swatches(&pixels, 3, 6))
    }

    /// A picture in the covers folder named after the playlist or its genre or decade. Only
    /// ever inside that folder, whatever the name contains.
    fn find_override(&self, shown: &str, lookup: &str) -> Option<PathBuf> {
        let dir = self.covers_directory.as_ref()?;
        if !dir.is_dir() {
            return None;
        }
        let root = full_path(dir);
        let mut stems = vec![safe_name(shown)];
        let other = safe_name(lookup);
        if !stems.contains(&other) {
            stems.push(other);
        }
        for stem in &stems {
            for extension in [".jpg", ".jpeg", ".png", ".webp"] {
                let path = full_path(&dir.join(format!("{stem}{extension}")));
                if path.starts_with(&root) && path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Someone's own picture, cropped to its centre square and sized like every cover.
    fn load_override(&self, path: &Path, size: i32) -> Option<Vec<u8>> {
        let image = match image::open(path) {
            Ok(image) => image,
            Err(e) => {
                warn!("The cover {} could not be read: {e}", path.display());
                return None;
            }
        };
        let side = image.width().min(image.height());
        let square = image.crop_imm(
            (image.width() - side) / 2,
            (image.height() - side) / 2,
            side,
            side,
        );
        let size = size.max(1) as u32;
        let sized = square.resize_exact(size, size, FilterType::CatmullRom);
        cover_image::encode_jpeg(&sized, JPEG_QUALITY)
    }
}

const POSSESSIVES: &[&str] = &["Your", "My", "Our"];

/// `n.ToString("N0", CultureInfo.InvariantCulture)`: thousands split by commas.
fn thousands(n: i32) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// A name made safe as a file name: no path separators or NULs (Linux's invalid file name
/// characters, plus both slashes), and no surrounding white space.
fn safe_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if matches!(c, '\0' | '/' | '\\') { '_' } else { c })
        .collect();
    replaced.trim().to_string()
}

/// `Path.GetFullPath`: absolute, with `.` and `..` worked out on the text alone (no links followed).
fn full_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
#[path = "list_cover_tests.rs"]
mod tests;
