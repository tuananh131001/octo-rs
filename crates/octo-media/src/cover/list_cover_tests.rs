//! octo.Tests/ListCoverTests.cs: the colour rules the covers share with the Octo apps and which
//! painted background a list's music gets (`CoverColourTests`); list covers as the server
//! serves them (`ListCoverTests`); and how quickly a cover draws (`CoverTimingTests`). The
//! golden covers (`CoverGoldenTests`) are in golden_tests.rs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use image::{ImageFormat, Rgb, RgbImage};

use super::*;
use crate::cover::cover_backgrounds;
use crate::cover::cover_book::CoverBook;
use crate::cover::cover_colours::{self, CoverMusic, Swatch};
use crate::cover::cover_fonts::{self, CoverTypesetter};
use crate::cover::cover_layout::{self, CoverAlign, WordsRole};
use crate::cover::cover_painter;
use crate::cover::test_support::{HEAVY, repo};

fn book() -> &'static CoverBook {
    CoverBook::shipped()
}

fn hex(text: &str) -> i32 {
    cover_colours::hex(text).expect("a colour")
}

// ================================================================ CoverColourTests

/// FNV-1a 64 over UTF-8, shifted right once, as the apps hash a list.
#[test]
fn cover_hash_is_fnv1a_shifted_right() {
    for (text, expected) in [
        ("", 0xcbf2_9ce4_8422_2325_u64 >> 1),
        ("a", 0xaf63_dc4c_8601_ec8c_u64 >> 1),
    ] {
        assert_eq!(expected as i64, cover_colours::cover_hash(text), "{text:?}");
    }
}

/// The design's check value for the pick hash, and the pick and turn it gives.
#[test]
fn cover_pick_is_fnv1a_then_fmix64_shifted_right() {
    assert_eq!(0x098f_28ee_76f6_47ce_i64, cover_colours::cover_pick("pl-1"));
    assert_eq!(15, cover_backgrounds::choose(book(), None, "pl-1"));
    assert_eq!(0, cover_backgrounds::orientation(book(), "pl-1"));
}

/// Numbered lists ("1" to "12", "p1" to "p12"), whose ids differ only in their last letters,
/// still look apart: with no music every one differs, the turns vary, and twelve lists of one
/// warm colour repeat a look at most twice.
#[test]
fn numbered_lists_look_apart() {
    let warm = CoverMusic::from_covers(&[vec![Swatch {
        argb: 0xFFE0_701F_u32 as i32,
        share: 1.0,
    }]]);
    let numbered: Vec<String> = (1..=12).map(|i| i.to_string()).collect();
    let prefixed: Vec<String> = (1..=12).map(|i| format!("p{i}")).collect();
    for ids in [numbered, prefixed] {
        let none: HashSet<(usize, i32)> = ids
            .iter()
            .map(|id| {
                (
                    cover_backgrounds::choose(book(), None, id),
                    cover_backgrounds::orientation(book(), id),
                )
            })
            .collect();
        assert_eq!(ids.len(), none.len(), "{}: looks without music", ids[0]);
        let turns: HashSet<i32> = ids
            .iter()
            .map(|id| cover_backgrounds::orientation(book(), id))
            .collect();
        assert!(turns.len() >= 5, "{}: {} turns", ids[0], turns.len());
        let picks: HashSet<(usize, i32)> = ids
            .iter()
            .map(|id| {
                (
                    cover_backgrounds::choose(book(), warm.as_ref(), id),
                    cover_backgrounds::orientation(book(), id),
                )
            })
            .collect();
        assert!(
            picks.len() >= ids.len() - 2,
            "{}: {} looks for warm music",
            ids[0],
            picks.len()
        );
    }
}

#[test]
fn lch_has_the_oklab_hue_of_a_colour() {
    for (text, hue) in [("#808080", 0.0), ("#ff0000", 29.2), ("#0000ff", 264.1)] {
        let lch = cover_colours::to_lch(hex(text));
        if lch.c > 0.01 {
            assert!((hue - 0.5..=hue + 0.5).contains(&lch.h), "{text}: hue {}", lch.h);
        } else {
            assert!(lch.c < 0.001, "{text}: chroma {}", lch.c);
        }
    }
}

#[test]
fn swatches_find_a_pictures_colours_by_share() {
    let pixels: Vec<i32> = (0..64 * 64)
        .map(|i| {
            if i % 4 == 0 {
                hex("#1d3f8c")
            } else {
                hex("#d9552b")
            }
        })
        .collect();

    let swatches = cover_colours::swatches(&pixels, 1, 6);

    assert_eq!(2, swatches.len());
    assert_eq!(hex("#d9552b"), swatches[0].argb);
    assert!(
        (0.74..=0.76).contains(&swatches[0].share),
        "share {}",
        swatches[0].share
    );
}

#[test]
fn music_from_colourful_covers_is_their_strongest_colour_rounded() {
    let warm = vec![
        vec![
            Swatch {
                argb: hex("#D9552B"),
                share: 0.6,
            },
            Swatch {
                argb: hex("#2B1A12"),
                share: 0.3,
            },
        ],
        vec![Swatch {
            argb: hex("#1D3F8C"),
            share: 0.5,
        }],
    ];

    let music = CoverMusic::from_covers(&warm).expect("colourful covers give a colour");
    let orange = cover_colours::to_lch(hex("#D9552B"));

    assert_eq!((orange.h + 0.5).floor() as i32 % 360, music.hue);
    assert_eq!((orange.c * 1000.0 + 0.5).floor() / 1000.0, music.chroma);
    assert_eq!((orange.l * 1000.0 + 0.5).floor() / 1000.0, music.lightness);
}

#[test]
fn music_from_grey_covers_or_none_is_nothing() {
    assert_eq!(
        None,
        CoverMusic::from_covers(&[vec![Swatch {
            argb: hex("#808080"),
            share: 0.9
        }]])
    );
    assert_eq!(None, CoverMusic::from_covers(&[]));
}

/// The picks and turns the design's rule gives, worked out apart from this code (by a short
/// script following cover-design.json "background" word for word), so the server and the apps
/// give a list the same background.
#[test]
fn background_is_the_designs_pick() {
    let cases: &[(&str, i32, f64, f64, &str, i32)] = &[
        ("Daft Punk Radio", 97, 0.143, 0.861, "tangerine.webp", 7),
        ("Rock Mix", 26, 0.14, 0.62, "afterglow.webp", 3),
        ("Your Mix", 262, 0.2, 0.5, "night-swim.webp", 4),
        ("1990s Mix", 134, 0.14, 0.62, "amber-night.webp", 0),
        ("Polka Mix", -1, 0.0, 0.0, "bubblegum.webp", 0),
        ("pl-1", 30, 0.1, 0.4, "coral.webp", 0),
        ("Daft Punk Radio", 261, 0.043, 0.722, "peach.webp", 7),
        ("Metal Mix", 40, 0.163, 0.601, "firewave.webp", 6),
    ];
    for &(id, hue, chroma, lightness, file, orientation) in cases {
        let music = (hue >= 0).then(|| CoverMusic::of(hue as f64, chroma, lightness));

        let chosen = &book().backgrounds[cover_backgrounds::choose(book(), music.as_ref(), id)];
        assert_eq!(file, chosen.file, "{id} with hue {hue}");
        assert_eq!(
            orientation,
            cover_backgrounds::orientation(book(), id),
            "{id}: turn"
        );
    }
}

/// Music too dull to say much (chroma under lowChromaAsGrey) picks as a list with no covers does.
#[test]
fn background_for_dull_music_is_the_names_pick() {
    let dull = CoverMusic::of(261.0, book().background_choice.low_chroma_as_grey - 0.001, 0.72);

    assert_eq!(
        cover_backgrounds::choose(book(), None, "Daft Punk Radio"),
        cover_backgrounds::choose(book(), Some(&dull), "Daft Punk Radio")
    );
}

/// Music of a colour gets one of a few backgrounds of that colour, the same one for the same list.
#[test]
fn background_matches_the_musics_colour() {
    for (r, g, b) in [
        (200, 30, 40),
        (30, 60, 200),
        (40, 160, 70),
        (240, 200, 30),
        (150, 40, 190),
    ] {
        let lch = cover_colours::to_lch(cover_colours::BLACK | (r << 16) | (g << 8) | b);
        let music = CoverMusic::of(lch.h, lch.c, lch.l);

        let picks: HashSet<usize> = (0..60)
            .map(|i| cover_backgrounds::choose(book(), Some(&music), &format!("pl-{i}")))
            .collect();

        let nearest = book().background_choice.nearest as usize;
        assert!(
            (2..=nearest).contains(&picks.len()),
            "({r},{g},{b}): {} picks",
            picks.len()
        );
        for pick in &picks {
            let background = &book().backgrounds[*pick];
            let nearest = background
                .hues()
                .iter()
                .map(|h| cover_colours::hue_distance(h.h, music.hue as f64))
                .fold(f64::INFINITY, f64::min);
            assert!(
                nearest < 50.0,
                "{} for hue {}: {nearest:.0}",
                background.name,
                music.hue
            );
        }
        assert_eq!(
            cover_backgrounds::choose(book(), Some(&music), "pl-1"),
            cover_backgrounds::choose(book(), Some(&music), "pl-1")
        );
    }
}

#[test]
fn background_without_music_is_any_one_always_the_same() {
    let picks: Vec<usize> = (0..400)
        .map(|i| cover_backgrounds::choose(book(), None, &format!("pl-{i}")))
        .collect();

    let again: Vec<usize> = (0..400)
        .map(|i| cover_backgrounds::choose(book(), None, &format!("pl-{i}")))
        .collect();
    assert_eq!(picks, again);
    let mut used: HashMap<usize, usize> = HashMap::new();
    for pick in &picks {
        *used.entry(*pick).or_default() += 1;
    }
    let count = book().backgrounds.len();
    assert!(
        used.len() >= count - 2,
        "{} of {count} backgrounds used",
        used.len()
    );
    assert!(
        used.values().all(|&n| n < 400 / count * 4),
        "a background used too often: {used:?}"
    );
}

/// The music colours the contact sheet's 24 lists get from its seed covers, or their genre's; -1 for none.
const SHEET_LISTS: &[(&str, i32, f64, f64)] = &[
    ("Daft Punk Radio", 261, 0.043, 0.722),
    ("Billie Eilish Radio", 63, 0.061, 0.575),
    ("Tame Impala Radio", 318, 0.041, 0.453),
    ("Radiohead Radio", 47, 0.159, 0.657),
    ("Kendrick Lamar Radio", 4, 0.068, 0.41),
    ("Your Mix", 241, 0.039, 0.732),
    ("Discovery Mix", 30, 0.225, 0.581),
    ("Bad Bunny Radio", 30, 0.225, 0.581),
    ("Jazz & Blues Mix", 225, 0.14, 0.62),
    ("Metal Mix", 40, 0.163, 0.601),
    ("1970s Mix", 75, 0.14, 0.62),
    ("Rock Mix", 26, 0.14, 0.62),
    ("Hip-Hop Mix", 61, 0.14, 0.62),
    ("1990s Mix", 134, 0.14, 0.62),
    ("2020s Mix", 168, 0.14, 0.62),
    ("Electronic Radio", 250, 0.14, 0.62),
    ("Polka Mix", -1, 0.0, 0.0),
    ("Red Hot Chili Peppers Radio", -1, 0.0, 0.0),
    (
        "The Most Unreasonably Long Playlist Name Anyone Ever Typed Into A Music Server Radio",
        -1,
        0.0,
        0.0,
    ),
    ("宇多田ヒカル Radio", -1, 0.0, 0.0),
    ("블랙핑크 BLACKPINK Radio", 5, 0.041, 0.336),
    ("فيروز Radio", 69, 0.065, 0.682),
    ("Late Night 🌙 Chill Mix", 206, 0.14, 0.62),
    ("Ünïcödé Café Mix", -1, 0.0, 0.0),
];

/// The contact sheet's 24 lists look apart: no background turned the same way twice, and none
/// used more than three times.
#[test]
fn sheet_lists_look_apart() {
    let looks: Vec<(usize, i32)> = SHEET_LISTS
        .iter()
        .map(|&(name, hue, chroma, lightness)| {
            let music = (hue >= 0).then(|| CoverMusic::of(hue as f64, chroma, lightness));
            (
                cover_backgrounds::choose(book(), music.as_ref(), name),
                cover_backgrounds::orientation(book(), name),
            )
        })
        .collect();

    let distinct: HashSet<&(usize, i32)> = looks.iter().collect();
    assert_eq!(looks.len(), distinct.len());
    let mut uses: HashMap<usize, usize> = HashMap::new();
    for (background, _) in &looks {
        *uses.entry(*background).or_default() += 1;
    }
    let (most, count) = uses.iter().max_by_key(|(_, n)| **n).expect("some backgrounds");
    assert!(
        *count <= 3,
        "{} is used {count} times",
        book().backgrounds[*most].name
    );
}

/// A turned background is the same pixels, quarter turned clockwise and then mirrored.
#[test]
fn turn_quarter_turns_clockwise_then_mirrors() {
    let _heavy = HEAVY.read();
    let plain = cover_backgrounds::load(book(), 0, 600).expect("loads");
    for v in 0..8 {
        let mut turned = cover_backgrounds::load(book(), 0, 600).expect("loads");
        cover_backgrounds::turn(&mut turned, v);
        for (x, y) in [(0u32, 0u32), (17, 250), (599, 3), (321, 598)] {
            // Where (x, y) of the turned picture came from.
            let (mut sx, mut sy) = (if v >= 4 { 599 - x } else { x }, y);
            for _ in 0..v % 4 {
                (sx, sy) = (sy, 599 - sx);
            }
            assert_eq!(
                plain.get_pixel(sx, sy),
                turned.get_pixel(x, y),
                "turn {v} at {x},{y}"
            );
        }
    }
    let mut turns: Vec<i32> = (0..400)
        .map(|i| cover_backgrounds::orientation(book(), &format!("pl-{i}")))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    turns.sort();
    assert_eq!((0..8).collect::<Vec<_>>(), turns);
}

/// Sizes other than the file's: halved while that leaves enough, then each pixel the mean of the area it covers.
#[test]
fn background_at_other_sizes_is_the_area_mean() {
    let _heavy = HEAVY.read();
    let at600 = cover_backgrounds::load(book(), 0, 600).expect("loads");
    let at800 = cover_backgrounds::load(book(), 0, 800).expect("loads");
    let at1200 = cover_backgrounds::load(book(), 0, 1200).expect("loads");

    // 800 from 1200: output pixel 1 covers source pixels 1.5 to 3, half of 1 and all of 2.
    let red = |image: &RgbImage, x: u32, y: u32| image.get_pixel(x, y).0[0] as f64;
    let (a, b) = (red(&at1200, 1, 0), red(&at1200, 2, 0));
    let (c, d) = (red(&at1200, 1, 1), red(&at1200, 2, 1));
    let want = (a * 0.5 / 1.5 + b / 1.5) * (1.0 / 1.5) + (c * 0.5 / 1.5 + d / 1.5) * (0.5 / 1.5);
    assert!(
        (red(&at800, 1, 0) - want).abs() <= 0.51,
        "{} against {want}",
        red(&at800, 1, 0)
    );
    let q = [
        red(&at1200, 0, 0),
        red(&at1200, 1, 0),
        red(&at1200, 0, 1),
        red(&at1200, 1, 1),
    ];
    assert_eq!(((q.iter().sum::<f64>() as u32 + 2) / 4) as f64, red(&at600, 0, 0));
}

#[test]
fn library_has_every_background_it_names_and_they_decode() {
    let _heavy = HEAVY.read();
    assert_eq!(48, book().backgrounds.len());
    for index in 0..book().backgrounds.len() {
        let image = cover_backgrounds::load(book(), index, 600).expect("loads");
        assert_eq!(600, image.width(), "{}", book().backgrounds[index].file);
    }
}

// ================================================================ ListCoverTests

/// A covers folder of its own, removed when the test ends.
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let root = tempfile::Builder::new()
            .prefix("octo-covers-")
            .tempdir()
            .expect("a temp dir");
        std::fs::create_dir_all(root.path().join("config").join("covers")).expect("the covers folder");
        Fixture { root }
    }

    fn covers_directory(&self) -> PathBuf {
        self.root.path().join("config").join("covers")
    }

    fn service(&self) -> CoverArtService {
        service_with(Some(self.covers_directory()))
    }
}

fn logo() -> PathBuf {
    repo("crates/octo/Assets/octo_logo.png")
}

fn service_with(covers: Option<PathBuf>) -> CoverArtService {
    CoverArtService::new(covers).with_logo_paths(vec![logo()])
}

fn decode(jpeg: &[u8]) -> RgbImage {
    image::load_from_memory(jpeg).expect("a cover decodes").to_rgb8()
}

fn pixel(jpeg: &[u8], x: u32, y: u32) -> Rgb<u8> {
    *decode(jpeg).get_pixel(x, y)
}

fn near(pixel: Rgb<u8>, colour: &str, tolerance: i32) -> bool {
    let expected = hex(colour);
    let channels = [
        cover_colours::r(expected),
        cover_colours::g(expected),
        cover_colours::b(expected),
    ];
    (0..3).all(|k| (pixel.0[k] as i32 - channels[k]).abs() <= tolerance)
}

/// Pixels in a square where two covers differ clearly, not just by JPEG noise.
fn changed(a: &[u8], b: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let (left, right) = (decode(a), decode(b));
    let mut changed = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let (p, q) = (left.get_pixel(x, y), right.get_pixel(x, y));
            if (0..3)
                .map(|k| (p.0[k] as i32 - q.0[k] as i32).abs())
                .max()
                .unwrap_or(0)
                > 40
            {
                changed += 1;
            }
        }
    }
    changed
}

fn rgb(colour: &str) -> Rgb<u8> {
    let c = hex(colour);
    Rgb([
        cover_colours::r(c) as u8,
        cover_colours::g(c) as u8,
        cover_colours::b(c) as u8,
    ])
}

fn save_png(path: &Path, width: u32, height: u32, colour: Rgb<u8>) {
    RgbImage::from_pixel(width, height, colour)
        .save_with_format(path, ImageFormat::Png)
        .expect("saves");
}

fn picture(colour: &str, second: Option<&str>) -> Vec<u8> {
    let mut image = RgbImage::from_pixel(120, 120, rgb(colour));
    if let Some(second) = second {
        for y in 80..120 {
            for x in 0..120 {
                image.put_pixel(x, y, rgb(second));
            }
        }
    }
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png).expect("encodes");
    bytes.into_inner()
}

fn seeds(pictures: Vec<Vec<u8>>) -> SeedsFn {
    let pictures = Arc::new(pictures);
    Arc::new(move || {
        let pictures = pictures.clone();
        Box::pin(async move {
            Ok(pictures
                .iter()
                .enumerate()
                .map(|(i, bytes)| {
                    let bytes = bytes.clone();
                    // C# named a seed by its length and fifth-last byte, which differed between
                    // ImageSharp's PNGs of two pictures; the image crate's PNGs of two flat
                    // pictures can share both, so a digest of the bytes keeps them apart.
                    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
                    });
                    let identity = format!("seed{i}-{}-{}-{digest:x}", bytes.len(), bytes[bytes.len() - 5]);
                    let fetch: FetchFn = Arc::new(move || {
                        let bytes = bytes.clone();
                        Box::pin(async move { Ok(Some(bytes)) })
                    });
                    CoverSeed { identity, fetch }
                })
                .collect())
        })
    })
}

fn list(name: &str, label: Option<&str>, kind: &str) -> ListCover {
    ListCover {
        name: name.to_string(),
        label: label.map(str::to_string),
        kind: kind.to_string(),
        seeds: None,
        song_count: None,
    }
}

// ------------------------------------------------------------ covers folder

#[test]
fn override_is_used_as_it_is_and_a_replacement_shows_without_a_restart() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let path = fixture.covers_directory().join("Rock Mix.png");
    save_png(&path, 120, 60, Rgb([0, 255, 0]));

    let first = service.get_named_cover("Rock Mix", Some("Rock"), None, list_kinds::MIX);
    assert_eq!(600, decode(&first).width());
    assert!(near(pixel(&first, 300, 300), "#00FF00", 24));

    save_png(&path, 60, 60, Rgb([0, 0, 255]));
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(SystemTime::now() + Duration::from_secs(60)))
        .expect("touches");
    assert!(near(
        pixel(
            &service.get_named_cover("Rock Mix", Some("Rock"), None, list_kinds::MIX),
            300,
            300
        ),
        "#0000FF",
        24
    ));
}

/// A picture in the covers folder beats colours from the music too.
#[tokio::test]
async fn override_beats_seed_colours_and_is_sized_as_asked() {
    let fixture = Fixture::new();
    save_png(
        &fixture.covers_directory().join("Daft Punk Radio.png"),
        60,
        60,
        Rgb([0, 255, 0]),
    );

    let mut daft_punk = list("Daft Punk Radio", None, list_kinds::RADIO);
    daft_punk.seeds = Some(seeds(vec![picture("#C08020", None)]));
    let bytes = fixture.service().get_list_cover(&daft_punk, Some(900)).await;

    let image = decode(&bytes);
    assert_eq!(900, image.width());
    assert!(near(*image.get_pixel(450, 450), "#00FF00", 24));
}

#[test]
fn override_by_genre_covers_every_list_of_it() {
    let fixture = Fixture::new();
    save_png(
        &fixture.covers_directory().join("Rock.png"),
        60,
        60,
        Rgb([0, 255, 0]),
    );

    let cover = fixture
        .service()
        .get_named_cover("Rock Mix", Some("Rock"), None, list_kinds::MIX);
    assert!(near(pixel(&cover, 300, 300), "#00FF00", 24));
}

/// Whatever a playlist is called, a cover is only ever read from the covers folder.
#[test]
fn override_never_reaches_outside_the_covers_folder() {
    let _heavy = HEAVY.read();
    let fixture = Fixture::new();
    let outside = fixture
        .covers_directory()
        .parent()
        .expect("a parent")
        .join("escape.png");
    save_png(&outside, 60, 60, Rgb([0, 255, 0]));

    let cover = fixture
        .service()
        .get_named_cover("../escape", None, None, list_kinds::MIX);
    assert!(!near(pixel(&cover, 300, 300), "#00FF00", 24));
}

// ------------------------------------------------------------ no badge

/// A station is a playlist like a mix: its cover is exactly the design, with nothing added.
/// The old badge sat top left at 28% of the cover; drawn on, it would show there.
#[test]
fn station_cover_is_the_design_alone_with_no_badge() {
    let _heavy = HEAVY.read();
    let fixture = Fixture::new();
    let service = fixture.service();
    let station = service.get_radio_station_cover("Rock Radio", None);
    let design = service
        .render(
            &CoverArtService::spec(
                "Rock Radio",
                Some(list_kinds::RADIO),
                None,
                service.fallback_music("Rock Radio", "Rock Radio"),
            ),
            600,
            true,
        )
        .expect("renders");

    assert_eq!(design, station);
    let badged = service.add_octo_badge(&station);
    assert!(
        changed(&station, &badged, 18, 18, 186, 186) > 1_000,
        "the badge should be visible when applied"
    );
}

#[test]
fn radio_station_covers_stay_plain_across_concurrent_first_requests() {
    let _heavy = HEAVY.read();
    let fixture = Fixture::new();
    let service = fixture.service();
    let names: Vec<String> = (0..16).map(|i| format!("Station {i} Radio")).collect();
    let covers: Vec<(String, Vec<u8>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = names
            .chunks(2)
            .map(|chunk| {
                let service = &service;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|name| (name.clone(), service.get_radio_station_cover(name, None)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("draws"))
            .collect()
    });

    let fresh = fixture.service();
    for (name, bytes) in &covers {
        assert_eq!(&fresh.get_radio_station_cover(name, None), bytes, "{name}");
    }
}

/// A picture someone chose for a station is theirs, and is not stamped.
#[test]
fn station_override_is_not_badged() {
    let fixture = Fixture::new();
    let service = fixture.service();
    save_png(
        &fixture.covers_directory().join("Rock Radio.png"),
        60,
        60,
        Rgb([0, 255, 0]),
    );

    assert_eq!(
        service.get_named_cover("Rock Radio", None, None, list_kinds::RADIO),
        service.get_radio_station_cover("Rock Radio", None)
    );
    assert!(near(
        pixel(&service.get_radio_station_cover("Rock Radio", None), 40, 40),
        "#00FF00",
        8
    ));
}

// ------------------------------------------------------------ palette sources

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn seed_covers_colour_the_cover_and_a_new_seed_redraws_it() {
    let _heavy = HEAVY.read();
    let fixture = Fixture::new();
    let service = fixture.service();
    let seeded = |colour: &str| {
        let mut daft_punk = list("Daft Punk Radio", None, list_kinds::RADIO);
        daft_punk.seeds = Some(seeds(vec![picture(colour, Some("#101010"))]));
        daft_punk
    };
    let plain = service
        .get_list_cover(&list("Daft Punk Radio", None, list_kinds::RADIO), None)
        .await;
    let gold = service.get_list_cover(&seeded("#C08020"), None).await;
    let gold_again = service.get_list_cover(&seeded("#C08020"), None).await;
    let teal = service.get_list_cover(&seeded("#1C8C8C"), None).await;

    assert_eq!(gold, gold_again);
    assert!(
        changed(&plain, &gold, 0, 0, 600, 600) > 10_000,
        "seed colours should change the cover"
    );
    assert!(
        changed(&gold, &teal, 0, 0, 600, 600) > 10_000,
        "a new seed cover should redraw it"
    );
}

/// Grey seeds, or seeds that cannot be fetched, leave the genre's colour.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn grey_or_missing_seeds_fall_back() {
    let _heavy = HEAVY.read();
    let fixture = Fixture::new();
    let service = fixture.service();
    let bare = service
        .get_list_cover(&list("Rock Mix", Some("Rock"), list_kinds::MIX), None)
        .await;
    let mut failing = list("Rock Mix", Some("Rock"), list_kinds::MIX);
    failing.seeds = Some(Arc::new(|| {
        Box::pin(async {
            let fetch: FetchFn = Arc::new(|| Box::pin(async { Err(anyhow::anyhow!("down")) }));
            Ok(vec![CoverSeed {
                identity: "gone".to_string(),
                fetch,
            }])
        })
    }));
    let failing = service.get_list_cover(&failing, None).await;
    let mut grey = list("Rock Mix", Some("Rock"), list_kinds::MIX);
    grey.seeds = Some(seeds(vec![picture("#808080", None)]));
    let grey = service.get_list_cover(&grey, None).await;

    assert_eq!(bare, failing);
    assert_eq!(bare, grey);
}

#[test]
fn genre_and_decade_give_their_hue_when_the_songs_give_none() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let rock = service
        .fallback_music("Rock Mix", "Rock")
        .expect("rock has a hue");
    let decade = service
        .fallback_music("1990s Mix", "1990s")
        .expect("the 1990s have a hue");

    assert_eq!(26, rock.hue);
    assert_eq!(134, decade.hue);
    assert_eq!(None, service.fallback_music("Polka Mix", "Polka"));
    assert_eq!(
        book().list_hue(Some("Soul Radio")),
        book().list_hue(Some("R&B & Soul"))
    );
}

// ------------------------------------------------------------ words

#[test]
fn spec_names_the_list_and_says_what_it_is() {
    let cases = [
        ("Daft Punk Radio", list_kinds::RADIO, "Daft Punk", "Station"),
        ("Rock Radio", list_kinds::RADIO, "Rock", "Station"),
        ("Late Night Jazz", list_kinds::RADIO, "Late Night Jazz", "Station"),
        ("Rock Mix", list_kinds::MIX, "Rock", "Mix"),
        ("1990s Mix", list_kinds::MIX, "1990s", "Mix"),
        ("Late Night Jazz", list_kinds::MIX, "Late Night Jazz", "Mix"),
    ];
    for (name, kind, title, line) in cases {
        let spec = CoverArtService::spec(name, Some(kind), Some(50), None);

        assert_eq!(title, spec.name, "{name} ({kind})");
        assert_eq!(Some(line), spec.line.as_deref(), "{name} ({kind})");
        assert_eq!(Some("50 songs"), spec.footer.as_deref(), "{name} ({kind})");
        assert_eq!(name, spec.id, "{name} ({kind})");
    }
}

/// A name that already ends in what it is gets no second line saying it again.
#[test]
fn spec_name_that_says_what_it_is_has_no_second_line() {
    let cases = [
        ("Your Mix", list_kinds::RADIO),
        ("Discovery Mix", list_kinds::RADIO),
        ("Your Mix", list_kinds::MIX),
        ("Late Night Radio Station", list_kinds::RADIO),
        ("Road Trip Playlist", list_kinds::MIX),
        ("Summer mixes", list_kinds::MIX),
        ("Pirate Radios", list_kinds::RADIO),
        ("Other Stations", list_kinds::RADIO),
        ("Old Playlists", list_kinds::MIX),
    ];
    for (name, kind) in cases {
        let spec = CoverArtService::spec(name, Some(kind), Some(50), None);

        assert_eq!(name, spec.name, "{name} ({kind})");
        assert_eq!(None, spec.line, "{name} ({kind})");
        let art = service_with(None).compose(&spec, 600);
        assert_eq!(2, art.words.len(), "{name} ({kind})");
        assert!(
            !art.words.iter().any(|w| w.text == "Station" || w.text == "Mix"),
            "{name} ({kind})"
        );
    }
}

/// Only the last word counts, and only a whole word.
#[test]
fn says_what_it_is_reads_the_last_whole_word() {
    let cases = [
        ("Mixtape Classics", false),
        ("Radiohead", false),
        ("Mix Masters", false),
        ("Your Mix", true),
        ("Discovery MIX", true),
    ];
    for (name, expected) in cases {
        assert_eq!(expected, cover_layout::says_what_it_is(name), "{name}");
    }
}

const NAMES: &[&str] = &[
    "Rock",
    "Red Hot Chili Peppers",
    "The Most Unreasonably Long Playlist Name Anyone Ever Typed Into A Music Server",
    "Supercalifragilisticexpialidociousness",
    "宇多田ヒカル",
    "블랙핑크 BLACKPINK",
    "فيروز",
    "שירים ישנים",
    "Late Night 🌙 Chill",
    "Ünïcödé Café",
];

/// Every word fits inside the margins, below the top, above the foot line.
#[test]
fn words_fit_inside_the_margins() {
    let fixture = Fixture::new();
    let service = fixture.service();
    for name in NAMES {
        for side in [600, 1200] {
            let art = service.compose(
                &CoverArtService::spec(&format!("{name} Radio"), Some(list_kinds::RADIO), Some(120), None),
                side,
            );
            let margin = (side as f32 * book().layout.margin).round();
            assert_eq!(3, art.words.len(), "{name} at {side}");
            for words in &art.words {
                let b = words.inked();
                assert!(
                    b[0] >= margin - 0.5 && b[2] <= side as f32 - margin + 0.5,
                    "{name} at {side}: {} runs {}..{}",
                    words.text,
                    b[0],
                    b[2]
                );
                assert!(
                    b[1] >= 0.0 && b[3] <= side as f32,
                    "{name} at {side}: {} rows {}..{}",
                    words.text,
                    b[1],
                    b[3]
                );
                assert!(
                    words.measured.lines <= book().layout.title.max_lines,
                    "{name} at {side}: lines"
                );
            }
            // The foot line comes first in the list; the name, then the line under it, then the
            // foot line, top to bottom.
            assert!(
                art.words[2].top >= art.words[1].inked()[3] - 0.5
                    && art.words[0].top >= art.words[2].inked()[3],
                "{name} at {side}: order"
            );
        }
    }
}

#[test]
fn long_names_wrap_onto_three_lines_at_most_and_the_longest_is_cut() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let wraps = service
        .compose(
            &CoverArtService::spec(
                "Red Hot Chili Peppers And Friends Radio",
                Some(list_kinds::RADIO),
                None,
                None,
            ),
            600,
        )
        .words
        .remove(0);
    let long = ["Unreasonably"; 12].join(" ");
    let cut = service
        .compose(
            &CoverArtService::spec(&long, Some(list_kinds::MIX), None, None),
            600,
        )
        .words
        .remove(0);

    assert!(
        (2..=3).contains(&wraps.measured.lines),
        "{} lines",
        wraps.measured.lines
    );
    assert!(!wraps.measured.cut);
    assert_eq!(3, cut.measured.lines);
    assert!(cut.measured.cut);
}

#[test]
fn right_to_left_names_are_set_from_the_right() {
    let fixture = Fixture::new();
    let words = fixture
        .service()
        .compose(
            &CoverArtService::spec("فيروز Radio", Some(list_kinds::RADIO), None, None),
            600,
        )
        .words;

    assert!(words.iter().all(|w| w.align == CoverAlign::Right));
    assert!(words[0].inked()[2] > 500.0);
}

/// Chinese, Japanese, Korean, Arabic, Hebrew and emoji names draw real letters, from a font
/// that has them: the name's box holds plenty of white, and each letter differs from the empty
/// box a missing glyph would leave.
#[test]
fn unicode_names_draw_their_letters() {
    let _heavy = HEAVY.read();
    let fallbacks = cover_fonts::fallbacks();
    if fallbacks.is_empty() {
        println!("skipped: no system fallback fonts are installed");
        return;
    }
    for name in ["宇多田ヒカル", "블랙핑크", "فيروز", "שירים", "🌙🎧"] {
        let font = cover_fonts::for_text(name, 600);
        let has_letters = name.chars().any(crate::cover::text::is_letter);
        if has_letters {
            assert_ne!(
                cover_fonts::for_weight(600),
                font.family,
                "{name} is set in Inter"
            );
        }
        let missing: Vec<char> = name
            .chars()
            .filter(|&cp| {
                !cover_fonts::has(&font.family, cp) && !fallbacks.iter().any(|f| cover_fonts::has(f, cp))
            })
            .collect();
        if !has_letters && !missing.is_empty() {
            // The Docker image and CI install Symbola for these; a desktop may not have it.
            println!("skipped {name}: no installed font draws {missing:?} (install Symbola)");
            continue;
        }
        assert!(
            missing.is_empty(),
            "no installed font draws {missing:?} in {name}"
        );

        let fixture = Fixture::new();
        let service = fixture.service();
        let spec = CoverArtService::spec(name, Some(list_kinds::MIX), None, None);
        let image = service.paint(&spec, 600, true).expect("paints");
        let b = service.compose(&spec, 600).words[0].inked();
        let mut white = 0;
        for y in b[1] as u32..b[3] as u32 {
            for x in b[0] as u32..b[2] as u32 {
                let p = image.get_pixel(x, y).0;
                if p[0] > 235 && p[1] > 235 && p[2] > 235 {
                    white += 1;
                }
            }
        }
        assert!(white > 1500, "{name}: only {white} white pixels in the name");
    }
}

// ------------------------------------------------------------ contrast

/// On every one of the 48 backgrounds, every pixel behind the words, as painted, reaches the
/// design's contrast against the words' own white: 3:1 at least for the large name and its
/// line (the veil aims for 4.5 and may stop at 3 to keep the colour), and 4.25:1 for the foot
/// line. The design sets the foot line's limit for 85% white over grey; over a vivid colour
/// that white blends a little darker, so on the most saturated backgrounds the foot line lands
/// between 4.29 and 4.5 rather than at 4.5.
#[test]
fn words_reach_their_contrast_on_every_pixel_behind_them() {
    let _heavy = HEAVY.read();
    let service = service_with(None);
    let spec = CoverArtService::spec(
        "Everything I Have Ever Loved Radio",
        Some(list_kinds::RADIO),
        Some(1234),
        None,
    );
    let backgrounds: Vec<usize> = (0..book().backgrounds.len()).collect();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4);
    std::thread::scope(|scope| {
        for chunk in backgrounds.chunks(backgrounds.len().div_ceil(threads)) {
            let (service, spec) = (&service, &spec);
            scope.spawn(move || {
                for &background in chunk {
                    for side in [600, 1200] {
                        let mut art = service.compose(spec, side);
                        art.background = background;
                        art.orientation = background as i32 % 8;
                        let backdrop =
                            cover_painter::paint(book(), &art, &CoverTypesetter, false).expect("paints");
                        for words in &art.words {
                            let need = if words.role == WordsRole::Footer {
                                4.25
                            } else {
                                3.0
                            };
                            let b = words.inked();
                            let mut worst = f64::MAX;
                            for y in (b[1].max(0.0) as u32)..(side as u32).min(b[3].ceil() as u32) {
                                for x in (b[0].max(0.0) as u32)..(side as u32).min(b[2].ceil() as u32) {
                                    let p = backdrop.get_pixel(x, y).0;
                                    let under = cover_colours::BLACK
                                        | (i32::from(p[0]) << 16)
                                        | (i32::from(p[1]) << 8)
                                        | i32::from(p[2]);
                                    let ratio = cover_colours::contrast_ratio(
                                        cover_colours::over(words.ink, under),
                                        under,
                                    );
                                    worst = worst.min(ratio);
                                }
                            }
                            assert!(
                                worst >= need,
                                "{} at {side}: '{}' reaches only {worst:.2}",
                                book().backgrounds[background].name,
                                words.text
                            );
                        }
                    }
                }
            });
        }
    });
}

/// After JPEG, which moves a level or two, the words still read behind them.
#[test]
fn words_still_read_after_jpeg() {
    let _heavy = HEAVY.read();
    let service = service_with(None);
    for background in ["lemonade.webp", "chiffon.webp", "peach.webp", "opal.webp"] {
        let index = book()
            .backgrounds
            .iter()
            .position(|b| b.file == background)
            .expect("in the library");
        let spec = CoverArtService::spec("Sunday Morning Radio", Some(list_kinds::RADIO), Some(99), None);
        let mut art = service.compose(&spec, 600);
        art.background = index;
        art.orientation = 0;
        let painted = cover_painter::paint(book(), &art, &CoverTypesetter, false).expect("paints");
        let jpeg = crate::cover::cover_image::encode_jpeg(&image::DynamicImage::ImageRgb8(painted), 92)
            .expect("encodes");
        let decoded = decode(&jpeg);
        for words in &art.words {
            let need = if words.role == WordsRole::Footer {
                4.15
            } else {
                2.9
            };
            let b = words.inked();
            for y in b[1] as u32..b[3] as u32 {
                for x in b[0] as u32..b[2] as u32 {
                    let p = decoded.get_pixel(x, y).0;
                    let under = cover_colours::BLACK
                        | (i32::from(p[0]) << 16)
                        | (i32::from(p[1]) << 8)
                        | i32::from(p[2]);
                    assert!(
                        cover_colours::contrast_ratio(cover_colours::over(words.ink, under), under) >= need,
                        "{background} at {x},{y}"
                    );
                }
            }
        }
    }
}

// ------------------------------------------------------------ determinism and speed

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn same_list_same_bytes_across_instances_and_sizes() {
    let _heavy = HEAVY.read();
    let fixture = Fixture::new();
    let mut tame_impala = list("Tame Impala Radio", None, list_kinds::RADIO);
    tame_impala.seeds = Some(seeds(vec![picture("#1D3F8C", Some("#D9552B"))]));
    tame_impala.song_count = Some(50);
    let a = fixture.service().get_list_cover(&tame_impala, Some(800)).await;
    let b = fixture.service().get_list_cover(&tame_impala, Some(800)).await;

    assert_eq!(a, b);
    assert_eq!(800, decode(&a).width());
    let small = fixture
        .service()
        .get_named_cover("x", None, Some(64), list_kinds::MIX);
    assert_eq!(Some((600, 600)), crate::cover::cover_image::measure(&small));
    let large = fixture
        .service()
        .get_named_cover("x", None, Some(5000), list_kinds::MIX);
    assert_eq!(Some((1200, 1200)), crate::cover::cover_image::measure(&large));
}

// ------------------------------------------------------------ contact sheet

/// A sheet of covers for looking at the design, only when asked:
/// OCTO_SHOTS_DIR=<folder> (and OCTO_SHOTS_SEEDS=<folder of album covers>) cargo test -p octo-media contact_sheet
#[tokio::test]
async fn contact_sheet() {
    let Some(output) = std::env::var_os("OCTO_SHOTS_DIR").filter(|v| !v.is_empty()) else {
        return;
    };
    let seed_dir = PathBuf::from(std::env::var_os("OCTO_SHOTS_SEEDS").unwrap_or_default());
    let seed = |stem: &str| std::fs::read(seed_dir.join(format!("{stem}.jpg"))).ok();
    use list_kinds::{MIX, RADIO};
    let lists: &[(&str, Option<&str>, &str, &[&str], Option<i32>)] = &[
        (
            "Daft Punk Radio",
            None,
            RADIO,
            &["Daft_Punk_Random_Access_Memories"],
            Some(50),
        ),
        (
            "Billie Eilish Radio",
            None,
            RADIO,
            &["Billie_Eilish_Happier_Than_Ever"],
            Some(50),
        ),
        (
            "Tame Impala Radio",
            None,
            RADIO,
            &["Tame_Impala_Currents"],
            Some(50),
        ),
        (
            "Radiohead Radio",
            None,
            RADIO,
            &["Radiohead_In_Rainbows"],
            Some(50),
        ),
        (
            "Kendrick Lamar Radio",
            None,
            RADIO,
            &["Kendrick_Lamar_DAMN"],
            Some(50),
        ),
        (
            "Your Mix",
            None,
            RADIO,
            &["Taylor_Swift_1989", "Frank_Ocean_Blonde"],
            Some(50),
        ),
        (
            "Discovery Mix",
            None,
            RADIO,
            &["Arctic_Monkeys_AM", "Bad_Bunny_Un_Verano_Sin_Ti"],
            Some(50),
        ),
        (
            "Bad Bunny Radio",
            None,
            RADIO,
            &["Bad_Bunny_Un_Verano_Sin_Ti"],
            Some(50),
        ),
        (
            "Jazz & Blues Mix",
            Some("Jazz & Blues"),
            MIX,
            &["Miles_Davis_Kind_of_Blue"],
            Some(100),
        ),
        (
            "Metal Mix",
            Some("Metal"),
            MIX,
            &["Metallica_Master_of_Puppets"],
            Some(100),
        ),
        (
            "1970s Mix",
            Some("1970s"),
            MIX,
            &["Fleetwood_Mac_Rumours"],
            Some(100),
        ),
        ("Rock Mix", Some("Rock"), MIX, &[], None),
        ("Hip-Hop Mix", Some("Hip-Hop"), MIX, &[], Some(100)),
        ("1990s Mix", Some("1990s"), MIX, &[], Some(100)),
        ("2020s Mix", Some("2020s"), MIX, &[], Some(100)),
        ("Electronic Radio", Some("electronic"), RADIO, &[], Some(50)),
        ("Polka Mix", Some("Polka"), MIX, &[], Some(37)),
        ("Red Hot Chili Peppers Radio", None, RADIO, &[], Some(50)),
        (
            "The Most Unreasonably Long Playlist Name Anyone Ever Typed Into A Music Server Radio",
            None,
            RADIO,
            &[],
            Some(50),
        ),
        (
            "宇多田ヒカル Radio",
            None,
            RADIO,
            &["Hikaru_Utada_Fantome"],
            Some(50),
        ),
        (
            "블랙핑크 BLACKPINK Radio",
            None,
            RADIO,
            &["BLACKPINK_The_Album"],
            Some(50),
        ),
        ("فيروز Radio", None, RADIO, &["Fairuz"], Some(50)),
        (
            "Late Night 🌙 Chill Mix",
            Some("Lo-fi & Chill"),
            MIX,
            &[],
            Some(100),
        ),
        ("Ünïcödé Café Mix", None, MIX, &[], Some(1)),
    ];
    let service = service_with(None);
    const SIDE: u32 = 600;
    const GAP: u32 = 40;
    const COLUMNS: u32 = 6;
    let rows = (lists.len() as u32).div_ceil(COLUMNS);
    let mut sheet = RgbImage::from_pixel(
        COLUMNS * (SIDE + GAP) + GAP,
        rows * (SIDE + GAP) + GAP,
        Rgb([12, 12, 13]),
    );
    for (i, (name, label, kind, stems, songs)) in lists.iter().enumerate() {
        let pictures: Vec<Vec<u8>> = stems.iter().filter_map(|stem| seed(stem)).collect();
        let mut cover = list(name, *label, kind);
        cover.seeds = (!pictures.is_empty()).then(|| seeds(pictures));
        cover.song_count = *songs;
        let watch = Instant::now();
        let bytes = service.get_list_cover(&cover, Some(SIDE as i32)).await;
        println!("{name}: {:.0} ms", watch.elapsed().as_secs_f64() * 1000.0);
        let i = i as u32;
        let (x, y) = (GAP + i % COLUMNS * (SIDE + GAP), GAP + i / COLUMNS * (SIDE + GAP));
        image::imageops::replace(&mut sheet, &decode(&bytes), i64::from(x), i64::from(y));
    }
    let output = PathBuf::from(output);
    std::fs::create_dir_all(&output).expect("the shots folder");
    sheet
        .save(output.join("list-covers.png"))
        .expect("saves the sheet");
}

// ================================================================ CoverTimingTests

/// Drawing a cover: background, veil, words and JPEG. The fastest of several runs is the
/// cover's own cost; the middle one also carries whatever else the machine is doing. Runs
/// alone (it holds the heavy-test lock for writing), so it measures the cover and not the suite.
#[test]
fn a_cover_draws_quickly() {
    let _alone = HEAVY.write();
    let service = service_with(None);
    let spec = CoverArtService::spec(
        "Red Hot Chili Peppers Radio",
        Some(list_kinds::RADIO),
        Some(100),
        None,
    );
    service.render(&spec, 600, true).expect("renders");
    service.render(&spec, 1200, true).expect("renders");

    let time = |side: i32| {
        let mut times: Vec<f64> = (0..9)
            .map(|i| {
                let mut spec = spec.clone();
                spec.id = format!("t{i}");
                let watch = Instant::now();
                service.render(&spec, side, true).expect("renders");
                watch.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(f64::total_cmp);
        (times[0], times[times.len() / 2])
    };

    let small = time(600);
    let large = time(1200);
    println!(
        "600 px: fastest {:.1} ms, middle {:.1} ms; 1200 px: fastest {:.1} ms, middle {:.1} ms",
        small.0, small.1, large.0, large.1
    );
    assert!(small.0 < 150.0, "600 px took {:.1} ms", small.0);
    assert!(large.0 < 450.0, "1200 px took {:.1} ms", large.0);
}
