//! The tag-dump differential check (PLAN Phase 5): for FLAC, MP3, M4A and Opus, every frame,
//! field and atom TagLibSharp writes must read back the same from the Rust writer.
//!
//! docs/rust-migration/fixtures/tags/generate.sh ran each scenario below with the C# code
//! (TagLibSharp 2.3.0) on the files in input/, and checked in each result and its dump in
//! csharp/. Two checks run against those dumps:
//!
//! - the Rust reader's dump of each C# file equals the C# dump: this port reads what TagLib#
//!   wrote as TagLib# reads it;
//! - the Rust writer, run through the same scenario on the same input, produces a file whose
//!   dump equals the C# dump: what lands on disk reads back the same.
//!
//! The dumps are compared after [`normalize`], which folds away only the differences listed in
//! known-diffs.md.

use std::path::{Path, PathBuf};

use octo_core::models::domain::Song;

use super::dump::dump;
use super::kept_identity::KeptIdentityTags;
use super::song_tags::{GenreWrite, embed_cover, write_song};
use super::tag_file::{TagFile, TagPicture};
use super::tag_writer_extras::{self as extras, TagFields};
use super::test_support::fixtures;
use crate::cover::cover_image;

const FORMATS: [&str; 4] = ["mp3", "flac", "m4a", "opus"];

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// The generator's `Scenarios.FullSong`.
fn full_song() -> Song {
    Song {
        title: "Teardrop".into(),
        artist: "Massive Attack feat. Elizabeth Fraser".into(),
        artists: strings(&["Massive Attack", "Elizabeth Fraser"]),
        primary_artist: Some("Massive Attack".into()),
        album: "Mezzanine".into(),
        album_artist: Some("Massive Attack".into()),
        track: Some(3),
        total_tracks: Some(11),
        disc_number: Some(1),
        year: Some(1998),
        bpm: Some(77),
        contributors: strings(&["Robert Del Naja", "Grant Marshall"]),
        copyright: Some("℗ 1998 Virgin Records".into()),
        music_brainz_recording_id: Some("5b0ef8e9-9b55-4a3b-9a6b-3a2f6b1c7d01".into()),
        music_brainz_release_group_id: Some("b2b4e1d0-6f5e-4a7c-8d9e-0f1a2b3c4d5e".into()),
        music_brainz_album_title: Some("Mezzanine".into()),
        music_brainz_artist_ids: strings(&[
            "10adbe5e-a2c0-4bf3-8249-2b4cbf6e6ca8",
            "2f5e3d3a-0000-4000-8000-000000000001",
        ]),
        is_compilation: true,
        isrc: Some("gb-aaa-98-00001".into()),
        label: Some("Virgin".into()),
        catalog_number: Some("CDV 2851".into()),
        barcode: Some("724384559922".into()),
        release_type: Some("album; compilation".into()),
        release_status: Some("official".into()),
        release_country: Some("GB".into()),
        original_date: Some("1998-04-20".into()),
        music_brainz_release_track_id: Some("8e7d6c5b-4a3f-4d2c-9b1a-0e9f8d7c6b5a".into()),
        music_brainz_album_artist_ids: strings(&["10adbe5e-a2c0-4bf3-8249-2b4cbf6e6ca8"]),
        acoust_id: Some("acoustid-1".into()),
        replay_gain_track_gain_db: Some(-6.52),
        replay_gain_track_peak: Some(0.891251),
        replay_gain_album_gain_db: Some(3.1),
        replay_gain_album_peak: Some(1.0),
        ..Default::default()
    }
}

fn open(path: &Path) -> TagFile {
    TagFile::open(path).unwrap_or_else(|e| panic!("{} opens: {e}", path.display()))
}

fn save(mut file: TagFile) {
    let path = file.path().to_path_buf();
    file.save()
        .unwrap_or_else(|e| panic!("{} saves: {e}", path.display()));
}

fn copy(from: &Path, to: &Path) -> PathBuf {
    std::fs::copy(from, to).expect("copied");
    to.to_path_buf()
}

/// Every scenario of the generator, run with the Rust writer into `dir`; the files written.
fn run_scenarios(dir: &Path) -> Vec<PathBuf> {
    let input = fixtures().join("input");
    let cover = std::fs::read(input.join("cover.png")).expect("cover.png");
    let mut written = Vec::new();
    for format in FORMATS {
        let fresh = |scenario: &str| {
            copy(
                &input.join(format!("input.{format}")),
                &dir.join(format!("{scenario}.{format}")),
            )
        };

        let full = fresh("full");
        let mut file = open(&full);
        write_song(
            &mut file,
            &full_song(),
            &GenreWrite::Write(strings(&["Trip Hop", "Rock"])),
        );
        embed_cover(&mut file, cover.clone(), cover_image::mime_type(&cover));
        save(file);

        let minimal = fresh("minimal");
        let mut file = open(&minimal);
        let song = Song {
            title: "Song".into(),
            artist: "Artist".into(),
            genre: Some("Pop".into()),
            ..Default::default()
        };
        write_song(&mut file, &song, &GenreWrite::Write(strings(&["Pop"])));
        save(file);

        let peer = fresh("peer");
        peer_scenario(&peer, &cover);

        let lyrics = fresh("lyrics");
        let mut file = open(&lyrics);
        file.set_lyrics(Some("[re:Octo]\n[00:01.00]Ça va — 東京\n[00:02.50]second line"));
        save(file);
        let unlyrics = copy(&lyrics, &dir.join(format!("unlyrics.{format}")));
        let mut file = open(&unlyrics);
        file.set_lyrics(None);
        save(file);

        let album_gain = copy(&full, &dir.join(format!("albumgain.{format}")));
        let mut file = open(&album_gain);
        extras::set_replay_gain(&mut file, None, None, Some(-7.25), Some(0.98765432));
        save(file);

        let cleared = copy(&full, &dir.join(format!("cleared.{format}")));
        let mut file = open(&cleared);
        file.set_genres(&[]);
        extras::set_compilation(&mut file, false);
        file.set_pictures(&[]);
        save(file);

        let kept_source = fresh("keptsrc");
        kept_source_scenario(&kept_source, format);
        let kept = copy(&full, &dir.join(format!("kept.{format}")));
        let identity = KeptIdentityTags::read(&kept_source, None).expect("the original reads");
        KeptIdentityTags::apply(&kept, &identity).expect("the identity is written");

        let exact = copy(&full, &dir.join(format!("exact.{format}")));
        let mut file = open(&exact);
        extras::set_exact(
            &mut file,
            TagFields::ALBUM_ARTISTS,
            &strings(&[" Massive Attack ", "", "Tracey Thorn"]),
        );
        extras::set_exact(&mut file, TagFields::ALBUM_VERSION, &strings(&["Deluxe"]));
        extras::set_exact(&mut file, TagFields::ALBUM_VERSION, &[]);
        extras::set_exact(&mut file, TagFields::BARCODE, &[]);
        extras::set_multi_value(&mut file, "ARTISTS", &strings(&[" A ", "", "B"]));
        extras::set_text(&mut file, TagFields::LABEL, Some("  Trimmed  "));
        extras::set_multi(
            &mut file,
            TagFields::ISRC,
            &strings(&["GBAAA9800001", "GBAAA9800002"]),
        );
        save(file);

        written.extend([
            full,
            minimal,
            peer,
            lyrics,
            unlyrics,
            album_gain,
            cleared,
            kept_source,
            kept,
            exact,
        ]);
    }
    written
}

/// The generator's `Scenarios.Peer`.
fn peer_scenario(path: &Path, cover: &[u8]) {
    let mut file = open(path);
    if path.extension().is_some_and(|ext| ext == "mp3") {
        let id3 = file.id3v2.as_mut().expect("an MP3 has an ID3v2 tag");
        id3.version = 3;
        id3.set_text_frame("TPE2", &["AC/DC"]);
    } else {
        file.set_album_artists(&strings(&["AC/DC"]));
    }
    file.set_title(Some("Ágætis byrjun — 東京"));
    file.set_performers(&strings(&["Sigur Rós"]));
    file.set_genres(&strings(&["Rock", "Shoegaze"]));
    file.set_track(7);
    file.set_disc(2);
    file.set_disc_count(2);
    file.set_year(1999);
    save(file);

    let mut file = open(path);
    extras::set_original_date(&mut file, Some("1999-06-12"));
    extras::set_text(&mut file, TagFields::LABEL, Some("  Smekkleysa Ünïcode  "));
    extras::set_multi(
        &mut file,
        TagFields::ARTIST_ID,
        &strings(&["a-1", "a-2", "a-1", " "]),
    );
    extras::set_multi_value(&mut file, "ARTISTS", &strings(&["Sigur Rós", "坂本龍一"]));
    extras::set_recording_id(&mut file, "rec-peer");
    extras::set_compilation(&mut file, false);
    file.set_lyrics(Some("plain peer lyrics\nwith ünïcode"));
    file.set_pictures(&[TagPicture::front_cover(cover.to_vec(), "image/png")]);
    save(file);
}

/// The generator's `Scenarios.KeptSource`.
fn kept_source_scenario(path: &Path, format: &str) {
    let mut file = open(path);
    file.set_title(Some("Teardrop"));
    file.set_album(Some("Mezzanine"));
    file.set_album_artists(&strings(&["Massive Attack", "Tracey Thorn"]));
    file.set_track(3);
    file.set_track_count(11);
    file.set_disc(1);
    file.set_disc_count(2);
    if format == "mp3" {
        file.id3v2
            .as_mut()
            .expect("an MP3 has an ID3v2 tag")
            .set_text_frame("TDRL", &["1998-04-20"]);
    } else {
        extras::set_text(&mut file, TagFields::RELEASE_DATE, Some("1998-04-20"));
    }
    extras::set_text(&mut file, TagFields::ALBUM_VERSION, Some("Original"));
    extras::set_text(
        &mut file,
        TagFields::ALBUM_ID,
        Some("1D2B6C3E-7A4F-4E1B-9C2D-3F4A5B6C7D8E"),
    );
    extras::set_release_track_id(&mut file, Some("{8E7D6C5B-4A3F-4D2C-9B1A-0E9F8D7C6B5A}"));
    extras::set_exact(
        &mut file,
        TagFields::ALBUM_ARTISTS,
        &strings(&["Massive Attack", "Tracey Thorn"]),
    );
    save(file);
}

/// The dumps with the known differences folded away (each one is in known-diffs.md):
///
/// - TagLib# writes a multi-value iTunes freeform atom once per value, each copy repeating
///   the mean and name before every value, and later updates only the first copy; lofty writes
///   one atom with one data box per value. Repeated mean/name entries go, and so does any atom
///   after the first with the same mean and name (the only one TagLib# reads).
/// - lofty takes a Vorbis comment's pictures out of the comment when it reads it, so where
///   METADATA_BLOCK_PICTURE stood among the fields is lost: the picture fields go last.
/// - lofty writes no comment block for a FLAC comment with no fields left, so the vendor
///   string goes with it: an empty comment's vendor is not compared.
fn normalize(dump: &str, flac: bool) -> String {
    let lines: Vec<&str> = dump.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut section = String::new();
    let mut dash_names: Vec<String> = Vec::new();
    let mut pictures: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<String>, pictures: &mut Vec<String>| out.append(pictures);
    for (at, line) in lines.iter().copied().enumerate() {
        if !line.starts_with("  ") {
            flush(&mut out, &mut pictures);
            section = line.split(' ').next().unwrap_or_default().to_string();
            let empty = lines.get(at + 1).is_none_or(|next| !next.starts_with("  "));
            if flac && section == "xiph" && empty {
                out.push("xiph (no fields)".into());
                continue;
            }
        }
        if section == "apple" && line.starts_with("  \"----\" [") {
            let body = &line["  \"----\" [".len()..line.len() - 1];
            let mut kept: Vec<&str> = Vec::new();
            for part in body.split(", ") {
                if (part.starts_with("mean ") || part.starts_with("name ")) && kept.contains(&part) {
                    continue;
                }
                kept.push(part);
            }
            let name = kept
                .iter()
                .take(2)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
                .to_lowercase();
            if dash_names.contains(&name) {
                continue;
            }
            dash_names.push(name);
            out.push(format!("  \"----\" [{}]", kept.join(", ")));
            continue;
        }
        if section == "xiph"
            && (line.starts_with("  METADATA_BLOCK_PICTURE ") || line.starts_with("  COVERART "))
        {
            pictures.push(line.to_string());
            continue;
        }
        out.push(line.to_string());
    }
    flush(&mut out, &mut pictures);
    out.join("\n")
}

/// lofty writes a character ID3v1's Latin-1 cannot hold as "?", where .NET's encoder picks a
/// look-alike ("—" as "-"): an ID3v1 line matches when it differs only where Rust wrote "?".
fn id3v1_matches(expected: &str, actual: &str) -> bool {
    expected.starts_with("id3v1 ")
        && expected.chars().count() == actual.chars().count()
        && expected
            .chars()
            .zip(actual.chars())
            .all(|(e, a)| e == a || a == '?')
}

/// Every difference between two dumps, as "name: line" pairs, for the failure message.
fn differences(name: &str, expected: &str, actual: &str) -> Option<String> {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<String> = actual
        .lines()
        .map(
            |line| match expected_lines.iter().find(|e| id3v1_matches(e, line)) {
                Some(e) => e.to_string(),
                None => line.to_string(),
            },
        )
        .collect();
    let actual_text = actual_lines.join("\n");
    if expected == actual_text {
        return None;
    }
    if let Ok(dir) = std::env::var("OCTO_TAG_DUMPS") {
        let _ = std::fs::write(Path::new(&dir).join(format!("{name}.rust")), &actual_text);
        let _ = std::fs::write(Path::new(&dir).join(format!("{name}.csharp")), expected);
    }
    let mut out = format!("--- {name}\n");
    for line in &expected_lines {
        if !actual_lines.iter().any(|a| a == line) {
            out.push_str(&format!("  C#:   {line}\n"));
        }
    }
    for line in &actual_lines {
        if !expected_lines.contains(&line.as_str()) {
            out.push_str(&format!("  Rust: {line}\n"));
        }
    }
    if out.lines().count() == 1 {
        out.push_str("  (same lines, different order)\n");
    }
    Some(out)
}

fn csharp_dump(name: &str) -> String {
    let path = fixtures().join("csharp").join(format!("{name}.dump"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_rust_reader_reads_the_csharp_files_as_taglib_reads_them() {
    let dir = fixtures().join("csharp");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("csharp/")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| FORMATS.iter().any(|format| name.ends_with(&format!(".{format}"))))
        .collect();
    names.sort();
    assert_eq!(names.len(), 40, "every scenario of every format");
    let failures: Vec<String> = names
        .iter()
        .filter_map(|name| {
            let expected = normalize(&csharp_dump(name), name.ends_with(".flac"));
            differences(
                name,
                &expected,
                &normalize(&dump(&dir.join(name)), name.ends_with(".flac")),
            )
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.concat());
}

#[test]
fn the_rust_writer_writes_what_the_csharp_writer_wrote() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let written = run_scenarios(dir.path());
    if let Ok(out) = std::env::var("OCTO_TAG_DUMPS") {
        for path in &written {
            let _ = std::fs::copy(path, Path::new(&out).join(path.file_name().expect("a file name")));
        }
    }
    let failures: Vec<String> = written
        .iter()
        .filter_map(|path| {
            let name = path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            let expected = normalize(&csharp_dump(&name), name.ends_with(".flac"));
            differences(&name, &expected, &normalize(&dump(path), name.ends_with(".flac")))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.concat());
}

/// An ID3v2 tag's version and frames (id, then flags and body bytes), straight from the file.
type RawId3 = (u8, Vec<(String, Vec<u8>)>);

fn id3v2_frames(bytes: &[u8]) -> Option<RawId3> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return None;
    }
    let version = bytes[3];
    let synchsafe = |b: &[u8]| {
        b.iter()
            .fold(0usize, |size, byte| (size << 7) | usize::from(*byte))
    };
    let size = synchsafe(&bytes[6..10]);
    let body = &bytes[10..(10 + size).min(bytes.len())];
    let mut frames = Vec::new();
    let mut at = 0;
    while at + 10 <= body.len() && body[at] != 0 {
        let id = String::from_utf8_lossy(&body[at..at + 4]).into_owned();
        let length = if version == 4 {
            synchsafe(&body[at + 4..at + 8])
        } else {
            u32::from_be_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]]) as usize
        };
        let end = (at + 10 + length).min(body.len());
        frames.push((id, body[at + 8..end].to_vec()));
        at = end;
    }
    Some((version, frames))
}

/// The ID3v1 tag at the end of a file.
fn id3v1(bytes: &[u8]) -> Option<&[u8]> {
    let tag = bytes.get(bytes.len().checked_sub(128)?..)?;
    tag.starts_with(b"TAG").then_some(tag)
}

/// On an MP3 the bytes themselves compare: every ID3v2 frame the Rust writer writes, flags,
/// encoding bytes, byte order marks and all, is the frame TagLib# wrote, in the same place.
/// Only the padding after the frames differs (lofty pads a rewritten tag its own way), a tag
/// left with no frames is dropped instead of written empty, and ID3v1's stand-in for a
/// character Latin-1 lacks is "?" (all three in known-diffs.md).
#[test]
fn the_rust_writer_writes_taglibs_id3v2_frames_byte_for_byte() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut failures = Vec::new();
    let written = run_scenarios(dir.path());
    for path in written
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "mp3"))
    {
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        let rust = std::fs::read(path).expect("the Rust file");
        let csharp = std::fs::read(fixtures().join("csharp").join(&name)).expect("the C# file");
        match (id3v2_frames(&csharp), id3v2_frames(&rust)) {
            (Some((_, frames)), None) if frames.is_empty() => {}
            (expected, actual) if expected == actual => {}
            (expected, actual) => {
                failures.push(format!("{name}: ID3v2\n  C#:   {expected:?}\n  Rust: {actual:?}"))
            }
        }
        let lossy =
            |e: &[u8], a: &[u8]| e.len() == a.len() && e.iter().zip(a).all(|(e, a)| e == a || *a == b'?');
        match (id3v1(&csharp), id3v1(&rust)) {
            (Some(e), Some(a)) if lossy(e, a) => {}
            (Some(e), None) if e[3..125].iter().all(|b| *b == 0) => {}
            (e, a) if e == a => {}
            (e, a) => failures.push(format!("{name}: ID3v1\n  C#:   {e:?}\n  Rust: {a:?}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
