//! The pure parts of `Services/Lyrics/LyricsLibrarySteps.cs`: the lyrics page's rules, without
//! the files and the lookups. The steps themselves are
//! `octo::services::lyrics::lyrics_library_steps`.

use std::collections::HashSet;

use super::lyrics_library_job::{LyricsLibraryMode, LyricsLibraryRow, LyricsLibraryRun, LyricsLibraryStatus};
use super::lyrics_models::{LyricsResult, LyricsTiming};
use super::song_lyrics::SongLyrics;
use crate::common::dotnet::is_null_or_white_space;

/// The reason a walk or a preview gives when it pauses at the services' wall.
pub const SERVICES_STOPPED_ANSWERING: &str =
    "The lyrics services stopped answering. Resume later to carry on from here.";

/// The reason a run stopped from the dashboard gives.
pub const STOPPED_FROM_THE_DASHBOARD: &str = "Stopped from the dashboard.";

/// The reason a run going when Octo stopped comes back with.
pub const OCTO_RESTARTED: &str = "Octo restarted while this run was in progress.";

/// What a row's `Has` calls a timing.
pub fn has_name(timing: LyricsTiming) -> &'static str {
    match timing {
        LyricsTiming::Plain => "plain",
        LyricsTiming::Line => "line",
        LyricsTiming::Word => "word",
        LyricsTiming::None => "none",
    }
}

/// One song as a scan sees it, once its tags are read: a row when its lyrics are missing, plain
/// or timed by line; word true when it already has word timing. Neither for a song without an
/// artist and a title to look it up by, or with a lyrics file Octo does not read.
pub fn scan_row(
    path: &str,
    artist: Option<&str>,
    title: Option<&str>,
    album: Option<&str>,
    has: impl FnOnce() -> SongLyrics,
) -> (Option<LyricsLibraryRow>, bool) {
    let (Some(artist), Some(title)) = (
        artist.filter(|artist| !is_null_or_white_space(Some(artist))),
        title.filter(|title| !is_null_or_white_space(Some(title))),
    ) else {
        return (None, false);
    };
    let has = has();
    if has.unknown {
        return (None, false);
    }
    if has.timing == LyricsTiming::Word {
        return (None, true);
    }
    let row = LyricsLibraryRow {
        id: LyricsLibraryRow::id_of(path),
        path: path.to_string(),
        artist: artist.trim().to_string(),
        title: title.trim().to_string(),
        album: album
            .filter(|album| !is_null_or_white_space(Some(album)))
            .map(|album| album.trim().to_string()),
        has: has_name(has.timing).to_string(),
        ..Default::default()
    };
    (Some(row), false)
}

/// What a preview offers for saving: lyrics a source found (not the song's own, not an
/// instrumental mark) that are timed better than what the song has.
pub fn better_than(found: Option<&LyricsResult>, has: LyricsTiming) -> Option<LyricsResult> {
    found
        .filter(|better| {
            !better.is_songs_own()
                && !better.instrumental
                && (better.has_synced() || better.has_plain())
                && better.timing() > has
        })
        .cloned()
}

/// The rows a Preview or a Save takes, in the list's order: the picked ones, and for a Save only
/// those a preview found lyrics for.
pub fn picked(rows: &[LyricsLibraryRow], wanted: Option<&[String]>, mode: LyricsLibraryMode) -> Vec<String> {
    let wanted: HashSet<&str> = wanted.unwrap_or_default().iter().map(String::as_str).collect();
    rows.iter()
        .filter(|row| {
            wanted.contains(row.id.as_str()) && (mode == LyricsLibraryMode::Preview || row.result == "found")
        })
        .map(|row| row.id.clone())
        .collect()
}

/// After an Undo: saved again with one press, if wanted, since the found lyrics are still on
/// the rows.
pub fn unsave_rows(run: &mut LyricsLibraryRun) {
    for row in run.rows.iter_mut().filter(|row| row.result == "saved") {
        row.result = "found".to_string();
    }
}

/// The reason an Undo finishes with: None when everything went back.
pub fn undo_reason(left: usize) -> Option<String> {
    (left > 0).then(|| format!("{left} changed since they were saved, so they were left alone."))
}

/// A run stopped from the dashboard.
pub fn cancel(run: &mut LyricsLibraryRun, now: chrono::DateTime<chrono::Utc>) {
    run.status = LyricsLibraryStatus::Cancelled;
    run.finished_utc = Some(now);
    run.reason = Some(STOPPED_FROM_THE_DASHBOARD.to_string());
}

/// A run that finished, with why when there is something to say.
pub fn finish(run: &mut LyricsLibraryRun, reason: Option<String>, now: chrono::DateTime<chrono::Utc>) {
    run.status = LyricsLibraryStatus::Completed;
    run.finished_utc = Some(now);
    run.reason = reason;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::SongLyricsPlace;

    fn line() -> SongLyrics {
        SongLyrics::new(SongLyricsPlace::Inside, LyricsTiming::Line, false, false)
    }

    #[test]
    fn scan_row_lists_missing_and_weaker_lyrics_only() {
        let (row, word) = scan_row("/m/a.mp3", Some(" Artist "), Some("Song "), Some("  "), line);
        let row = row.expect("a row");
        assert!(!word);
        assert_eq!(
            (
                row.artist.as_str(),
                row.title.as_str(),
                row.album.as_deref(),
                row.has.as_str()
            ),
            ("Artist", "Song", None, "line")
        );
        assert_eq!(row.result, "weak");
        assert_eq!(row.id, LyricsLibraryRow::id_of("/m/a.mp3"));

        let word = || SongLyrics::new(SongLyricsPlace::Beside, LyricsTiming::Word, true, false);
        assert_eq!(
            scan_row("/m/a.mp3", Some("A"), Some("S"), None, word),
            (None, true)
        );
        let unknown = || SongLyrics::new(SongLyricsPlace::Beside, LyricsTiming::None, false, true);
        assert_eq!(
            scan_row("/m/a.mp3", Some("A"), Some("S"), None, unknown),
            (None, false)
        );
        // No artist or no title: the song's lyrics are never even looked at.
        let never = || -> SongLyrics { panic!("not read") };
        assert_eq!(scan_row("/m/a.mp3", None, Some("S"), None, never), (None, false));
        assert_eq!(
            scan_row("/m/a.mp3", Some("A"), Some(" "), None, never),
            (None, false)
        );
    }

    #[test]
    fn better_than_takes_only_better_timed_lyrics_from_a_source() {
        let words = LyricsResult::new(
            "KuGou",
            Some("[00:01.00]<00:01.00>w<00:02.00>".into()),
            None,
            false,
        );
        let lines = LyricsResult::new("LRCLIB", Some("[00:01.00]other lines".into()), None, false);
        assert_eq!(better_than(Some(&words), LyricsTiming::Line), Some(words.clone()));
        assert_eq!(better_than(Some(&lines), LyricsTiming::Line), None);
        assert_eq!(better_than(Some(&lines), LyricsTiming::Plain), Some(lines));
        assert_eq!(
            better_than(
                Some(&LyricsResult::songs_own(LyricsTiming::Word)),
                LyricsTiming::None
            ),
            None
        );
        assert_eq!(
            better_than(
                Some(&LyricsResult::new("x", None, None, true)),
                LyricsTiming::None
            ),
            None
        );
        assert_eq!(better_than(None, LyricsTiming::None), None);
    }

    #[test]
    fn picked_keeps_the_lists_order_and_saves_only_found_rows() {
        let row = |id: &str, result: &str| LyricsLibraryRow {
            id: id.into(),
            result: result.into(),
            ..Default::default()
        };
        let rows = [row("a", "weak"), row("b", "found"), row("c", "none")];
        let wanted = ["c".to_string(), "b".to_string(), "x".to_string()];
        assert_eq!(
            picked(&rows, Some(&wanted), LyricsLibraryMode::Preview),
            ["b", "c"]
        );
        assert_eq!(picked(&rows, Some(&wanted), LyricsLibraryMode::Save), ["b"]);
        assert!(picked(&rows, None, LyricsLibraryMode::Preview).is_empty());
    }

    #[test]
    fn undo_turns_saved_rows_back_into_found_ones() {
        let mut run = LyricsLibraryRun {
            rows: vec![
                LyricsLibraryRow {
                    result: "saved".into(),
                    ..Default::default()
                },
                LyricsLibraryRow {
                    result: "kept".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        unsave_rows(&mut run);
        assert_eq!(run.rows[0].result, "found");
        assert_eq!(run.rows[1].result, "kept");
        assert_eq!(undo_reason(0), None);
        assert_eq!(
            undo_reason(2).as_deref(),
            Some("2 changed since they were saved, so they were left alone.")
        );
    }
}
