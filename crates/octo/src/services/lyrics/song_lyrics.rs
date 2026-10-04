//! Port of the file-reading half of `Services/Lyrics/SongLyrics.cs` (`Of`, `MayWriteInside`,
//! `MayWriteBeside`), and of `LyricsSidecarWriter.IsOctos`, which it calls. The answer and the
//! timing rules are `octo_core::lyrics::song_lyrics`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use octo_core::common::dotnet::is_null_or_white_space;
use octo_core::lyrics::song_lyrics::OCTO_MARK;
use octo_core::lyrics::{SongLyrics, SongLyricsPlace};

use crate::services::state_file;

/// The lyrics a song file has now: a .lrc beside it first, then a .txt, then its tags.
pub fn of(audio_path: &Path) -> SongLyrics {
    of_inner(audio_path, None, true)
}

/// The same, with the lyrics in the song's tags already read (a scan reads every tag once).
pub fn of_with_tag_lyrics(audio_path: &Path, tag_lyrics: Option<&str>) -> SongLyrics {
    of_inner(audio_path, tag_lyrics, false)
}

fn of_inner(audio_path: &Path, tag_lyrics: Option<&str>, read_tags: bool) -> SongLyrics {
    let stem = stem(audio_path);
    let lrc = with_suffix(&stem, ".lrc");
    if lrc.exists() {
        return SongLyrics::new(
            SongLyricsPlace::Beside,
            SongLyrics::timing_of(read_quietly(&lrc).as_deref()),
            is_octos(&lrc),
            false,
        );
    }
    let txt = with_suffix(&stem, ".txt");
    if txt.exists() {
        return SongLyrics::new(
            SongLyricsPlace::Beside,
            SongLyrics::timing_of(read_quietly(&txt).as_deref()),
            false,
            false,
        );
    }
    if has_other_lyrics_file(&stem) {
        return SongLyrics::new(SongLyricsPlace::Beside, SongLyrics::timing_of(None), false, true);
    }
    let read;
    let inside = if read_tags {
        read = read_tag_lyrics(audio_path).ok().flatten();
        read.as_deref()
    } else {
        tag_lyrics
    };
    let Some(inside) = inside.filter(|text| !is_null_or_white_space(Some(text))) else {
        return SongLyrics::NOTHING;
    };
    SongLyrics::new(
        SongLyricsPlace::Inside,
        SongLyrics::timing_of(Some(inside)),
        SongLyrics::is_octos_text(inside),
        false,
    )
}

/// Whether Octo may write lyrics in the song's tags: they hold none, or Octo's.
pub fn may_write_inside(audio_path: &Path) -> bool {
    match read_tag_lyrics(audio_path) {
        Ok(lyrics) => match lyrics.as_deref() {
            Some(text) if !is_null_or_white_space(Some(text)) => SongLyrics::is_octos_text(text),
            _ => true,
        },
        Err(_) => false,
    }
}

/// Whether Octo may write a lyrics file beside the song: a .lrc for timed lyrics where there
/// is none or Octo's, a .txt for plain ones only where there is no lyrics file at all.
pub fn may_write_beside(stem: &Path, timed: bool) -> bool {
    if has_other_lyrics_file(stem) {
        return false;
    }
    let lrc = with_suffix(stem, ".lrc");
    if timed {
        return !lrc.exists() || is_octos(&lrc);
    }
    !lrc.exists() && !with_suffix(stem, ".txt").exists()
}

/// `LyricsSidecarWriter.IsOctos`: whether a .lrc's first line (spaces and byte order mark
/// aside) is Octo's mark, so Octo wrote it and may replace it.
pub fn is_octos(lrc_path: &Path) -> bool {
    let Ok(text) = state_file::read_text(lrc_path) else {
        return false;
    };
    // StreamReader.ReadLine: the first line ends at "\r" or "\n".
    let first = text.split(['\r', '\n']).next().unwrap_or("");
    first.trim().trim_start_matches('\u{FEFF}') == OCTO_MARK
}

/// The song's path without its extension: where its lyrics files sit, as `<stem>.lrc`.
pub fn stem(audio_path: &Path) -> PathBuf {
    audio_path.with_extension("")
}

fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut path: OsString = stem.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

fn has_other_lyrics_file(stem: &Path) -> bool {
    SongLyrics::OTHER_EXTENSIONS
        .iter()
        .any(|extension| with_suffix(stem, extension).exists())
}

fn read_quietly(path: &Path) -> Option<String> {
    state_file::read_text(path).ok()
}

/// The lyrics in the song's own tags (TagLib's `Tag.Lyrics`), or an error when the file cannot
/// be read as audio.
// STUB(3-D tags): replaced when the tags port lands. Until then a song's tags are taken to hold
// no lyrics; a file that cannot be opened at all is still an error.
fn read_tag_lyrics(audio_path: &Path) -> std::io::Result<Option<String>> {
    std::fs::metadata(audio_path).map(|_| None)
}

#[cfg(test)]
mod tests {
    use octo_core::lyrics::LyricsTiming;

    use super::*;

    fn song(dir: &Path) -> PathBuf {
        let audio = dir.join("Artist - Song.mp3");
        std::fs::write(&audio, b"not really audio").expect("written");
        audio
    }

    #[test]
    fn an_lrc_beside_the_song_comes_first_and_says_whether_it_is_octos() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let audio = song(dir.path());
        std::fs::write(dir.path().join("Artist - Song.txt"), "plain").expect("written");
        std::fs::write(
            dir.path().join("Artist - Song.lrc"),
            "\u{FEFF}[re:Octo]\r\n[00:01.00]<00:01.00>word",
        )
        .expect("written");

        assert_eq!(
            of(&audio),
            SongLyrics::new(SongLyricsPlace::Beside, LyricsTiming::Word, true, false)
        );
        // The owner's .lrc is not Octo's.
        std::fs::write(dir.path().join("Artist - Song.lrc"), "[00:01.00]line").expect("written");
        assert_eq!(
            of(&audio),
            SongLyrics::new(SongLyricsPlace::Beside, LyricsTiming::Line, false, false)
        );
    }

    #[test]
    fn a_txt_then_an_unknown_lyrics_file_then_the_tags() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let audio = song(dir.path());
        assert_eq!(of(&audio), SongLyrics::NOTHING);
        assert_eq!(
            of_with_tag_lyrics(&audio, Some("[re:Octo]\n[00:01.00]x")),
            SongLyrics::new(SongLyricsPlace::Inside, LyricsTiming::Line, true, false)
        );
        assert_eq!(of_with_tag_lyrics(&audio, Some("  ")), SongLyrics::NOTHING);

        std::fs::write(dir.path().join("Artist - Song.ttml"), "<tt/>").expect("written");
        assert_eq!(
            of_with_tag_lyrics(&audio, Some("words")),
            SongLyrics::new(SongLyricsPlace::Beside, LyricsTiming::None, false, true)
        );

        std::fs::write(dir.path().join("Artist - Song.txt"), "plain words").expect("written");
        assert_eq!(
            of(&audio),
            SongLyrics::new(SongLyricsPlace::Beside, LyricsTiming::Plain, false, false)
        );
    }

    #[test]
    fn may_write_beside_never_replaces_the_owners_files() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let audio = song(dir.path());
        let stem = stem(&audio);
        assert!(may_write_beside(&stem, true));
        assert!(may_write_beside(&stem, false));

        std::fs::write(with_suffix(&stem, ".txt"), "plain").expect("written");
        assert!(may_write_beside(&stem, true));
        assert!(!may_write_beside(&stem, false));

        std::fs::write(with_suffix(&stem, ".lrc"), "[re:Octo]\n[00:01.00]x").expect("written");
        assert!(may_write_beside(&stem, true));
        std::fs::write(with_suffix(&stem, ".lrc"), "[00:01.00]the owner's").expect("written");
        assert!(!may_write_beside(&stem, true));

        std::fs::remove_file(with_suffix(&stem, ".lrc")).expect("removed");
        std::fs::write(with_suffix(&stem, ".yml"), "x").expect("written");
        assert!(!may_write_beside(&stem, true));
    }

    #[test]
    fn a_file_that_cannot_be_opened_may_not_be_written_inside() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(!may_write_inside(&dir.path().join("missing.mp3")));
        assert!(may_write_inside(&song(dir.path())));
        assert!(!is_octos(&dir.path().join("missing.lrc")));
    }
}
