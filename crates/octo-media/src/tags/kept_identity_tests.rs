//! Port of `octo.Tests/KeptIdentityTests.cs`.
//!
//! W8: a replacement carries the tags Navidrome builds the song's ids from, read the way
//! Navidrome reads them, so it takes the original's place instead of arriving as a new song.
//! Everything else the new tagging wrote stays.

use std::path::{Path, PathBuf};

use super::kept_identity::KeptIdentityTags;
use super::tag_file::TagFile;
use super::tag_writer_extras::{self as extras, TagFields};
use super::test_support::{flac, mp3, write};
use crate::audio::test_support::{require_ffmpeg, run_ffmpeg};

const ALBUM_ID: &str = "1d2b6c3e-7a4f-4e1b-9c2d-3f4a5b6c7d8e";
const TRACK_ID: &str = "8e7d6c5b-4a3f-4d2c-9b1a-0e9f8d7c6b5a";
const NEW_ALBUM_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const NEW_TRACK_ID: &str = "11111111-2222-4333-8444-555555555555";

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn open(path: &Path) -> TagFile {
    TagFile::open(path).expect("the file opens")
}

fn new_mp3(dir: &Path, name: &str) -> PathBuf {
    write(dir, &format!("{name}.mp3"), &mp3())
}

fn new_flac(dir: &Path, name: &str) -> PathBuf {
    write(dir, &format!("{name}.flac"), &flac())
}

fn tag_as_the_new_pipeline_would(path: &Path) {
    let mut file = open(path);
    file.set_title(Some("Teardrop (2019 Remaster)"));
    file.set_album(Some("Mezzanine (Deluxe)"));
    file.set_album_artists(&strings(&["Massive Attack"]));
    file.set_year(2019);
    file.set_genres(&strings(&["Trip Hop"]));
    extras::set_recording_id(&mut file, "rec-teardrop");
    extras::set_text(&mut file, TagFields::ISRC, Some("GBAAA9800001"));
    extras::set_replay_gain(&mut file, Some(-6.52), Some(0.891251), None, None);
    extras::set_release_track_id(&mut file, Some(NEW_TRACK_ID));
    extras::set_text(&mut file, TagFields::ALBUM_ID, Some(NEW_ALBUM_ID));
    file.save().expect("saved");
}

fn assert_the_new_tagging_survived(path: &Path) {
    let mut file = open(path);
    assert_eq!(
        extras::read_recording_id(&mut file).as_deref(),
        Some("rec-teardrop")
    );
    assert_eq!(
        extras::read_text(&file, TagFields::ISRC).as_deref(),
        Some("GBAAA9800001")
    );
    assert_eq!(
        extras::read_text(&file, TagFields::TRACK_GAIN).as_deref(),
        Some("-6.52 dB")
    );
    assert_eq!(file.genres(), strings(&["Trip Hop"]));
}

fn pid_inputs_of(path: &Path) -> String {
    KeptIdentityTags::pid_inputs(&KeptIdentityTags::read(path, None).expect("the file reads"))
}

#[test]
fn mp3_to_flac_the_replacement_reads_as_the_original() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let original = new_mp3(dir.path(), "original");
    {
        let mut file = open(&original);
        file.set_title(Some("Teardrop"));
        file.set_album(Some("Mezzanine"));
        file.set_album_artists(&strings(&["Massive Attack"]));
        file.set_track(3);
        file.set_track_count(11);
        file.set_disc(1);
        file.id3v2
            .as_mut()
            .expect("an MP3 has an ID3v2 tag")
            .set_text_frame("TDRL", &["1998-04-20"]);
        extras::set_text(&mut file, TagFields::ALBUM_VERSION, Some("Original"));
        extras::set_text(&mut file, TagFields::ALBUM_ID, Some(&ALBUM_ID.to_uppercase()));
        extras::set_release_track_id(&mut file, Some(TRACK_ID));
        file.save().expect("saved");
    }
    let replacement = new_flac(dir.path(), "replacement");
    tag_as_the_new_pipeline_would(&replacement);
    let identity = KeptIdentityTags::read(&original, None).expect("the original reads");
    KeptIdentityTags::apply(&replacement, &identity).expect("applied");
    assert_eq!(
        (
            identity.album_id.as_deref(),
            identity.release_track_id.as_deref(),
            identity.release_date.as_deref()
        ),
        (Some(ALBUM_ID), Some(TRACK_ID), Some("1998-04-20"))
    );
    assert_eq!(
        KeptIdentityTags::pid_inputs(&identity),
        pid_inputs_of(&replacement)
    );
    assert_the_new_tagging_survived(&replacement);
}

#[test]
fn flac_to_flac_what_the_original_lacked_is_removed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let original = new_flac(dir.path(), "original");
    {
        let mut file = open(&original);
        file.set_title(Some("Teardrop"));
        file.set_album(Some("Mezzanine"));
        file.set_album_artists(&strings(&["Massive Attack"]));
        file.save().expect("saved");
    }
    let replacement = new_flac(dir.path(), "replacement");
    tag_as_the_new_pipeline_would(&replacement);
    {
        let mut file = open(&replacement);
        let xiph = file.xiph.as_mut().expect("a Vorbis comment");
        xiph.set_field("ALBUMVERSION", &["Deluxe"]);
        xiph.set_field("MUSICBRAINZ_ALBUMCOMMENT", &["remaster"]);
        xiph.set_field("RELEASEDATE", &["2019-01-01"]);
        xiph.set_field("YEAR", &["2019"]);
        xiph.set_field("ALBUM ARTIST", &["Someone Else"]);
        file.save().expect("saved");
    }
    let identity = KeptIdentityTags::read(&original, None).expect("the original reads");
    KeptIdentityTags::apply(&replacement, &identity).expect("applied");
    let after = open(&replacement);
    let fields = after.xiph.as_ref().expect("a Vorbis comment").fields.keys();
    for gone in [
        "MUSICBRAINZ_ALBUMID",
        "MUSICBRAINZ_RELEASETRACKID",
        "ALBUMVERSION",
        "MUSICBRAINZ_ALBUMCOMMENT",
        "RELEASEDATE",
        "YEAR",
        "ALBUM ARTIST",
    ] {
        assert!(
            !fields.iter().any(|field| field.eq_ignore_ascii_case(gone)),
            "{gone} is gone"
        );
    }
    assert_eq!(after.album_artists(), strings(&["Massive Attack"]));
    assert_the_new_tagging_survived(&replacement);
}

#[test]
fn mp3_to_flac_the_release_track_id_the_new_tagging_added_is_removed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let original = new_mp3(dir.path(), "original");
    {
        let mut file = open(&original);
        file.set_title(Some("Teardrop"));
        file.set_album(Some("Mezzanine"));
        file.save().expect("saved");
    }
    let replacement = new_flac(dir.path(), "replacement");
    tag_as_the_new_pipeline_would(&replacement);
    let identity = KeptIdentityTags::read(&original, None).expect("the original reads");
    KeptIdentityTags::apply(&replacement, &identity).expect("applied");
    let after = open(&replacement);
    assert_eq!(extras::read_text(&after, TagFields::RELEASE_TRACK_ID), None);
    assert_eq!(
        KeptIdentityTags::read(&replacement, None)
            .expect("reads")
            .release_track_id,
        None
    );
}

#[test]
fn m4a_to_flac_the_date_atom_counts_as_the_release_date() {
    require_ffmpeg!();
    let dir = tempfile::tempdir().expect("a temp dir");
    let original = dir.path().join("original.m4a");
    run_ffmpeg(
        dir.path(),
        "-f lavfi -i sine=frequency=440:duration=1 -c:a aac -b:a 128k original.m4a",
    );
    {
        let mut file = open(&original);
        file.set_title(Some("Teardrop"));
        file.set_album(Some("Mezzanine"));
        file.set_album_artists(&strings(&["Massive Attack"]));
        file.set_year(1998);
        extras::set_text(&mut file, TagFields::ALBUM_ID, Some(ALBUM_ID));
        file.save().expect("saved");
    }
    let replacement = new_flac(dir.path(), "replacement");
    tag_as_the_new_pipeline_would(&replacement);
    let identity = KeptIdentityTags::read(&original, None).expect("the original reads");
    KeptIdentityTags::apply(&replacement, &identity).expect("applied");
    assert_eq!(
        (identity.release_date.as_deref(), identity.album_id.as_deref()),
        (Some("1998"), Some(ALBUM_ID))
    );
    assert_eq!(
        KeptIdentityTags::pid_inputs(&identity),
        pid_inputs_of(&replacement)
    );
    assert_the_new_tagging_survived(&replacement);
}

#[test]
fn no_album_artist_takes_the_name_navidrome_gave_the_album() {
    let cases = [
        ("Massive Attack feat. Tracey Thorn", false, None, "Massive Attack"),
        ("Massive Attack", true, None, "Various Artists"),
        (
            "Massive Attack",
            false,
            Some("Massive Attack & Friends"),
            "Massive Attack & Friends",
        ),
    ];
    for (artist, compilation, navidrome, expected) in cases {
        let dir = tempfile::tempdir().expect("a temp dir");
        let original = new_flac(dir.path(), "original");
        {
            let mut file = open(&original);
            file.set_title(Some("T"));
            file.set_performers(&strings(&[artist]));
            extras::set_compilation(&mut file, compilation);
            file.save().expect("saved");
        }
        let identity = KeptIdentityTags::read(&original, navidrome).expect("the original reads");
        assert_eq!(
            identity.album_artist,
            strings(&[expected]),
            "{artist} / {compilation} / {navidrome:?}"
        );
    }
}

/// Pin: TagLib# splits a version 3 TPE2 at "/", Navidrome's reader does not.
#[test]
fn a_version3_album_artist_with_a_slash_stays_one_name() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let original = new_mp3(dir.path(), "original");
    {
        let mut file = open(&original);
        let id3 = file.id3v2.as_mut().expect("an MP3 has an ID3v2 tag");
        id3.version = 3;
        id3.set_text_frame("TPE2", &["AC/DC"]);
        file.save().expect("saved");
    }
    assert_eq!(
        KeptIdentityTags::read(&original, None)
            .expect("reads")
            .album_artist,
        strings(&["AC/DC"])
    );
}

#[test]
fn navidrome_date_matches_parse_date() {
    let cases = [
        ("1998-04-20T10:00:00", Some("1998-04-20")),
        ("1998-04", Some("1998-04")),
        ("1998", Some("1998")),
        ("April 1998", Some("1998")),
        ("1998-13-01", Some("1998")),
        ("98", None),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            KeptIdentityTags::navidrome_date(raw).as_deref(),
            expected,
            "{raw}"
        );
    }
}

#[test]
fn navidrome_uuid_is_canonical_or_nothing() {
    assert_eq!(
        KeptIdentityTags::navidrome_uuid(&format!("{{{}}}", ALBUM_ID.to_uppercase())).as_deref(),
        Some(ALBUM_ID)
    );
    assert_eq!(
        KeptIdentityTags::navidrome_uuid(&format!("urn:uuid:{ALBUM_ID}")).as_deref(),
        Some(ALBUM_ID)
    );
    assert_eq!(KeptIdentityTags::navidrome_uuid("not-an-id"), None);
}
