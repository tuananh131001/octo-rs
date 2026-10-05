//! Port of `octo.Tests/TagWriterExtrasTests.cs`.
//!
//! Every new field lands in the exact frame Picard writes and the library server reads, on an
//! MP3 and on a FLAC: the raw frames are read back, not TagLib's properties, since four of
//! TagLib's own names differ from Picard's. A new ID3 tag is version 4 so the original date has
//! its own frame; a tag a file arrived with keeps its version.

use std::path::{Path, PathBuf};

use super::id3::Id3Tag;
use super::tag_file::TagFile;
use super::tag_writer_extras::{self as extras, TagFields};
use super::test_support::{flac, mp3, write};

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn new_mp3(dir: &Path) -> PathBuf {
    write(dir, "song.mp3", &mp3())
}

fn new_flac(dir: &Path) -> PathBuf {
    write(dir, "song.flac", &flac())
}

fn open(path: &Path) -> TagFile {
    TagFile::open(path).expect("the file opens")
}

fn write_the_set(path: &Path) {
    let mut file = open(path);
    file.set_title(Some("Teardrop"));
    extras::set_text(&mut file, TagFields::ISRC, Some("GBAAA9800001"));
    extras::set_text(&mut file, TagFields::LABEL, Some("Virgin"));
    extras::set_text(&mut file, TagFields::CATALOG_NUMBER, Some("CDV 2851"));
    extras::set_text(&mut file, TagFields::BARCODE, Some("724384559922"));
    extras::set_multi(&mut file, TagFields::RELEASE_TYPE, &strings(&["album"]));
    extras::set_text(&mut file, TagFields::RELEASE_STATUS, Some("official"));
    extras::set_text(&mut file, TagFields::RELEASE_COUNTRY, Some("GB"));
    extras::set_original_date(&mut file, Some("1998-04-20"));
    extras::set_release_track_id(&mut file, Some("t-mezz-3"));
    extras::set_multi(&mut file, TagFields::ALBUM_ARTIST_ID, &strings(&["a-ma"]));
    extras::set_multi(&mut file, TagFields::ARTIST_ID, &strings(&["a-ma", "a-ef"]));
    extras::set_text(&mut file, TagFields::FINGERPRINT_ID, Some("acoustid-1"));
    extras::set_replay_gain(&mut file, Some(-6.52), Some(0.891251), None, None);
    file.save().expect("saved");
}

fn txxx(id3: &Id3Tag, description: &str) -> Option<String> {
    id3.user_text(description, true)
        .and_then(|values| values.into_iter().next())
}

fn text(id3: &Id3Tag, frame: &str) -> Option<String> {
    id3.text_values(frame).into_iter().next()
}

#[test]
fn mp3_every_field_lands_in_picards_frame_and_the_new_tag_is_version4() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_mp3(dir.path());
    write_the_set(&path);

    let file = open(&path);
    let id3 = file.id3v2.as_ref().expect("an ID3v2 tag");
    assert_eq!(id3.version, 4);
    assert_eq!(text(id3, "TSRC").as_deref(), Some("GBAAA9800001"));
    assert_eq!(text(id3, "TPUB").as_deref(), Some("Virgin"));
    assert_eq!(txxx(id3, "CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(txxx(id3, "BARCODE").as_deref(), Some("724384559922"));
    assert_eq!(txxx(id3, "MusicBrainz Album Type").as_deref(), Some("album"));
    assert_eq!(txxx(id3, "MusicBrainz Album Status").as_deref(), Some("official"));
    assert_eq!(
        txxx(id3, "MusicBrainz Album Release Country").as_deref(),
        Some("GB")
    );
    assert_eq!(text(id3, "TDOR").as_deref(), Some("1998-04-20"));
    assert_eq!(
        txxx(id3, "MusicBrainz Release Track Id").as_deref(),
        Some("t-mezz-3")
    );
    assert_eq!(txxx(id3, "MusicBrainz Album Artist Id").as_deref(), Some("a-ma"));
    assert_eq!(
        id3.user_text("MusicBrainz Artist Id", true),
        Some(strings(&["a-ma", "a-ef"]))
    );
    assert_eq!(txxx(id3, "Acoustid Id").as_deref(), Some("acoustid-1"));
    assert_eq!(txxx(id3, "REPLAYGAIN_TRACK_GAIN").as_deref(), Some("-6.52 dB"));
    assert_eq!(txxx(id3, "REPLAYGAIN_TRACK_PEAK").as_deref(), Some("0.891251"));
    assert_eq!(txxx(id3, "REPLAYGAIN_ALBUM_GAIN"), None);
    // TagLib's own readers agree on the fields it names the same way.
    assert_eq!(file.isrc().as_deref(), Some("GBAAA9800001"));
    assert_eq!(file.publisher().as_deref(), Some("Virgin"));
}

#[test]
fn flac_every_field_lands_in_picards_vorbis_name() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_flac(dir.path());
    write_the_set(&path);

    let file = open(&path);
    let xiph = file.xiph.as_ref().expect("a Vorbis comment");
    let first = |key: &str| xiph.first_field(key);
    assert_eq!(first("ISRC").as_deref(), Some("GBAAA9800001"));
    assert_eq!(first("LABEL").as_deref(), Some("Virgin"));
    assert_eq!(first("CATALOGNUMBER").as_deref(), Some("CDV 2851"));
    assert_eq!(first("BARCODE").as_deref(), Some("724384559922"));
    assert_eq!(first("RELEASETYPE").as_deref(), Some("album"));
    assert_eq!(first("RELEASESTATUS").as_deref(), Some("official"));
    assert_eq!(first("RELEASECOUNTRY").as_deref(), Some("GB"));
    assert_eq!(first("ORIGINALDATE").as_deref(), Some("1998-04-20"));
    assert_eq!(first("ORIGINALYEAR").as_deref(), Some("1998"));
    assert_eq!(first("MUSICBRAINZ_RELEASETRACKID").as_deref(), Some("t-mezz-3"));
    assert_eq!(first("MUSICBRAINZ_ALBUMARTISTID").as_deref(), Some("a-ma"));
    assert_eq!(xiph.field("MUSICBRAINZ_ARTISTID"), strings(&["a-ma", "a-ef"]));
    assert_eq!(first("ACOUSTID_ID").as_deref(), Some("acoustid-1"));
    assert_eq!(first("REPLAYGAIN_TRACK_GAIN").as_deref(), Some("-6.52 dB"));
    assert_eq!(first("REPLAYGAIN_TRACK_PEAK").as_deref(), Some("0.891251"));
    // Not TagLib's own spellings, which Picard does not read.
    assert_eq!(first("ORGANIZATION"), None);
    assert_eq!(first("MUSICBRAINZ_ALBUMTYPE"), None);
}

/// A peer's version 3 tag stays version 3 and gets the original year in TORY.
#[test]
fn mp3_existing_version3_tag_keeps_its_version_and_gets_tory() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_mp3(dir.path());
    {
        let mut file = open(&path);
        file.id3v2.as_mut().expect("an MP3 has an ID3v2 tag").version = 3;
        file.set_title(Some("From a peer"));
        file.save().expect("saved");
    }
    {
        let mut file = open(&path);
        extras::set_original_date(&mut file, Some("1998-04-20"));
        extras::set_text(&mut file, TagFields::LABEL, Some("Virgin"));
        file.save().expect("saved");
    }

    let reopened = open(&path);
    let tag = reopened.id3v2.as_ref().expect("an ID3v2 tag");
    assert_eq!(tag.version, 3);
    // TagLib reads a version 3 TORY back under its version 4 id, so the frame on disk is what
    // proves the version 3 spelling: the bytes carry TORY, not TDOR.
    assert_eq!(text(tag, "TDOR").as_deref(), Some("1998"));
    assert_eq!(text(tag, "TPUB").as_deref(), Some("Virgin"));
    let bytes = std::fs::read(&path).expect("read");
    let head = String::from_utf8_lossy(&bytes[..2048.min(bytes.len())]).into_owned();
    assert!(head.contains("TORY"));
    assert!(!head.contains("TDOR"));
}

#[test]
fn set_text_empty_value_writes_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_flac(dir.path());
    {
        let mut file = open(&path);
        extras::set_text(&mut file, TagFields::LABEL, Some("Peer's Label"));
        file.save().expect("saved");
    }
    {
        let mut file = open(&path);
        extras::set_text(&mut file, TagFields::LABEL, Some(""));
        extras::set_text(&mut file, TagFields::BARCODE, None);
        extras::set_replay_gain(&mut file, None, None, None, None);
        file.save().expect("saved");
    }
    let reopened = open(&path);
    assert_eq!(
        extras::read_text(&reopened, TagFields::LABEL).as_deref(),
        Some("Peer's Label")
    );
    assert_eq!(extras::read_text(&reopened, TagFields::BARCODE), None);
    assert_eq!(extras::read_text(&reopened, TagFields::TRACK_GAIN), None);
}

#[test]
fn gain_text_always_signed_two_decimals_and_a_dot() {
    for (gain, expected) in [(-6.52, "-6.52 dB"), (3.1, "+3.10 dB"), (0.0, "+0.00 dB")] {
        assert_eq!(extras::gain_text(gain), expected, "gain {gain}");
    }
}

#[test]
fn peak_text_six_decimals() {
    assert_eq!(extras::peak_text(0.966051), "0.966051");
}

#[test]
fn read_facts_round_trips_what_was_written() {
    for format in ["mp3", "flac"] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = if format == "mp3" {
            new_mp3(dir.path())
        } else {
            new_flac(dir.path())
        };
        {
            let mut file = open(&path);
            file.set_title(Some("Teardrop"));
            file.set_performers(&strings(&["Massive Attack"]));
            file.set_album(Some("Mezzanine"));
            file.set_album_artists(&strings(&["Massive Attack"]));
            file.set_year(1998);
            file.set_track(3);
            file.set_disc(1);
            file.set_music_brainz_release_id(Some("r-mezz"));
            extras::set_text(&mut file, TagFields::ISRC, Some("GBAAA9800001"));
            extras::set_text(&mut file, TagFields::LABEL, Some("Virgin"));
            extras::set_text(&mut file, TagFields::CATALOG_NUMBER, Some("CDV 2851"));
            extras::set_text(&mut file, TagFields::BARCODE, Some("724384559922"));
            extras::set_recording_id(&mut file, "rec-teardrop");
            extras::set_compilation(&mut file, true);
            file.save().expect("saved");
        }

        let facts = extras::read_facts(path.to_str().expect("a UTF-8 path"), true);

        assert!(facts.tags_are_evidence, "{format}");
        assert_eq!(facts.extension, format!(".{format}"));
        assert_eq!(facts.title.as_deref(), Some("Teardrop"), "{format}");
        assert_eq!(facts.artist.as_deref(), Some("Massive Attack"), "{format}");
        assert_eq!(facts.album.as_deref(), Some("Mezzanine"), "{format}");
        assert_eq!(facts.album_artist.as_deref(), Some("Massive Attack"), "{format}");
        assert_eq!(facts.year, Some(1998), "{format}");
        assert_eq!(facts.track, Some(3), "{format}");
        assert_eq!(facts.disc, Some(1), "{format}");
        assert_eq!(facts.isrcs, strings(&["GBAAA9800001"]), "{format}");
        assert_eq!(facts.barcode.as_deref(), Some("724384559922"), "{format}");
        assert_eq!(facts.catalog_number.as_deref(), Some("CDV 2851"), "{format}");
        assert_eq!(facts.label.as_deref(), Some("Virgin"), "{format}");
        assert_eq!(facts.recording_id.as_deref(), Some("rec-teardrop"), "{format}");
        assert_eq!(facts.release_id.as_deref(), Some("r-mezz"), "{format}");
        assert!(facts.is_compilation, "{format}");
        assert_eq!(facts.sample_rate, 44100, "{format}");
    }
}

/// An uploader's name is not a credit: a staged upload gives only its length and format.
#[test]
fn read_facts_tags_not_evidence_reads_only_length_and_format() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_mp3(dir.path());
    {
        let mut file = open(&path);
        file.set_title(Some("Some Channel - Song"));
        file.set_album(Some("Some Channel"));
        file.save().expect("saved");
    }

    let facts = extras::read_facts(path.to_str().expect("a UTF-8 path"), false);

    assert!(!facts.tags_are_evidence);
    assert_eq!(facts.title, None);
    assert_eq!(facts.album, None);
    assert_eq!(facts.extension, ".mp3");
}

#[test]
fn read_facts_unreadable_file_is_unknown_not_a_throw() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = write(dir.path(), "garbage.flac", &[1, 2, 3]);
    let facts = extras::read_facts(path.to_str().expect("a UTF-8 path"), true);
    assert_eq!(facts.duration_seconds, 0);
    assert!(!facts.tags_are_evidence);
}

/// Rust-only: `TagWriterExtras` is the release identifier's `FileFactsReader`.
#[test]
fn the_facts_reader_is_read_facts() {
    use octo_core::tagging::FileFactsReader;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_flac(dir.path());
    let facts = extras::TagWriterExtras.read_facts(path.to_str().expect("a UTF-8 path"), false);
    // The fixture has no audio after its STREAMINFO, so it has no length, as TagLib# read it.
    assert_eq!(
        (
            facts.duration_seconds,
            facts.sample_rate,
            facts.extension.as_str()
        ),
        (0, 44100, ".flac")
    );
}

/// Rust-only: a FLAC has its STREAMINFO length only when audio follows the metadata. TagLib#'s
/// `StreamHeader.Duration` is zero for an empty stream, which the download tagging tests'
/// fixture is, and lofty would otherwise read the two seconds STREAMINFO claims.
#[test]
fn a_flac_has_a_length_only_when_it_has_audio() {
    crate::audio::test_support::require_ffmpeg!();
    let dir = tempfile::tempdir().expect("a temp dir");
    crate::audio::test_support::run_ffmpeg(
        dir.path(),
        "-f lavfi -i sine=frequency=440:duration=2 -c:a flac tone.flac",
    );
    assert_eq!(open(&dir.path().join("tone.flac")).duration_seconds(), 2);
    assert_eq!(open(&new_flac(dir.path())).duration_seconds(), 0);
}

// ---- From `DownloadPlacementTests` (task 4-B): the recording id and the artists ---------------

/// TagLib# after 2.3.0 writes Tag.MusicBrainzTrackId as the release TRACK id on ID3. Writing the
/// UFID frame directly keeps the recording id where Navidrome reads it whatever the package.
#[test]
fn set_recording_id_mp3_writes_the_music_brainz_org_ufid() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_mp3(dir.path());
    let mut file = open(&path);
    extras::set_recording_id(&mut file, "rec-1");
    file.save().expect("saved");

    let mut read = open(&path);
    let id3 = read.id3v2.as_ref().expect("an ID3v2 tag");
    assert_eq!(id3.ufid("http://musicbrainz.org"), Some(&b"rec-1"[..]));
    assert_eq!(extras::read_recording_id(&mut read).as_deref(), Some("rec-1"));
}

#[test]
fn set_recording_id_flac_writes_musicbrainz_trackid() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = new_flac(dir.path());
    let mut file = open(&path);
    extras::set_recording_id(&mut file, "rec-1");
    file.save().expect("saved");

    let read = open(&path);
    let xiph = read.xiph.as_ref().expect("a Vorbis comment");
    assert_eq!(xiph.first_field("MUSICBRAINZ_TRACKID").as_deref(), Some("rec-1"));
}

#[test]
fn set_multi_value_writes_one_value_per_artist() {
    for name in ["t.mp3", "t.flac"] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = if name.ends_with(".mp3") {
            write(dir.path(), name, &mp3())
        } else {
            write(dir.path(), name, &flac())
        };
        let mut file = open(&path);
        extras::set_multi_value(&mut file, "ARTISTS", &strings(&["Bizarrap", "Rauw Alejandro"]));
        file.save().expect("saved");

        let read = open(&path);
        let values = if name.ends_with(".mp3") {
            read.id3v2
                .as_ref()
                .and_then(|id3| id3.user_text("ARTISTS", false))
                .expect("a user text frame")
        } else {
            read.xiph.as_ref().expect("a Vorbis comment").field("ARTISTS")
        };
        assert_eq!(values, strings(&["Bizarrap", "Rauw Alejandro"]), "{name}");
    }
}
