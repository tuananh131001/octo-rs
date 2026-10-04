//! `PathHelperTests.cs`, `PathHelperLayoutTests.cs`, and the `PathHelper` half of
//! `SoulseekResolveRetryTests.cs` (`WindowsDrivePathDetectionTests`).

use super::*;

// ---- PathHelperTests: BuildTrackPath is what the Organized folder layout is built on, so an
// album lands as one folder on disk instead of a folder per track. ----

// Relative so the test reads the same on Windows and on the Linux CI runner.
const ROOT: &str = "musicroot";

#[test]
fn build_track_path_groups_by_album_and_prefixes_track_number() {
    let path = PathHelper::build_track_path(
        ROOT,
        "Boards Of Canada",
        "In A Beautiful Place Out In The Country",
        "Kid For Today",
        Some(1),
        ".mp3",
    );

    assert_eq!(
        path,
        "musicroot/Boards Of Canada/In A Beautiful Place Out In The Country/01 - Kid For Today.mp3"
    );
}

#[test]
fn build_track_path_pads_track_number_to_two_digits() {
    let path = PathHelper::build_track_path(ROOT, "A", "B", "C", Some(7), ".flac");
    assert!(path.ends_with("07 - C.flac"), "{path}");
}

#[test]
fn build_track_path_double_digit_track_is_not_padded_further() {
    let path = PathHelper::build_track_path(ROOT, "A", "B", "C", Some(12), ".flac");
    assert!(path.ends_with("12 - C.flac"), "{path}");
}

#[test]
fn build_track_path_no_track_number_omits_prefix() {
    // A standalone single has no position; it must not get a "00 - " prefix.
    let path = PathHelper::build_track_path(ROOT, "A", "B", "Song", None, ".mp3");
    assert!(path.ends_with("Song.mp3"), "{path}");
    assert!(!path.contains(" - Song.mp3"), "{path}");
}

#[test]
fn build_track_path_empty_extension_produces_no_trailing_dot() {
    // The yt-dlp shim appends .mp3 itself, so it is handed a path with no extension.
    let path = PathHelper::build_track_path(ROOT, "A", "B", "Song", Some(3), "");
    assert!(path.ends_with("03 - Song"), "{path}");
}

#[test]
fn build_track_path_sanitizes_path_hostile_names() {
    for nasty in ["AC/DC", "Sigur Rós: Ágætis", "What?<>|", "Nul\0Byte"] {
        let path = PathHelper::build_track_path(ROOT, nasty, "Album", "Title", Some(1), ".mp3");

        // Assert the invariant, not a literal: which characters are illegal differs by
        // platform (Windows rejects : ? < > |, Linux only / and NUL).
        let segments: Vec<&str> = path.split('/').collect();

        // An artist containing a slash must not silently become an extra directory level.
        assert_eq!(segments.len(), 4, "{nasty}: {path}");
        assert_eq!(segments[0], ROOT, "{nasty}");
        assert_eq!(segments[2], "Album", "{nasty}");
        assert_eq!(segments[3], "01 - Title.mp3", "{nasty}");

        for c in INVALID_FILE_NAME_CHARS {
            assert!(
                !segments[1].contains(c),
                "{nasty}: artist segment kept illegal char {}",
                c as u32
            );
        }
    }
}

#[test]
fn build_track_path_accented_characters_survive() {
    // Sanitizing must not mangle legitimate non-ASCII names.
    let path = PathHelper::build_track_path(
        ROOT,
        "Sigur Rós",
        "Ágætis Byrjun",
        "Svefn-g-englar",
        Some(2),
        ".mp3",
    );

    assert!(path.contains("Sigur Rós"), "{path}");
    assert!(path.contains("Ágætis Byrjun"), "{path}");
    assert!(path.ends_with("02 - Svefn-g-englar.mp3"), "{path}");
}

#[test]
fn build_track_path_blank_album_or_artist_falls_back_to_unknown() {
    let path = PathHelper::build_track_path(ROOT, "", "   ", "Song", None, ".mp3");
    assert!(path.contains("Unknown"), "{path}");
}

#[test]
fn sanitize_folder_name_trims_trailing_dots() {
    // Windows silently drops trailing dots on folder names.
    assert_eq!(PathHelper::sanitize_folder_name("Album..."), "Album");
}

/// Every one of these names a different recording. Naming used to strip all brackets, so a
/// live take and the studio cut shared one path and the second download deleted the first.
#[test]
fn file_title_keeps_what_names_a_different_recording() {
    for (title, expected) in [
        ("Song (Live)", "Song (Live)"),
        ("Song [Remix]", "Song [Remix]"),
        ("Song (feat. Guest)", "Song (feat. Guest)"),
        ("Song (Acoustic Version)", "Song (Acoustic Version)"),
    ] {
        assert_eq!(PathHelper::file_title(title, "Artist"), expected, "{title}");
    }
}

#[test]
fn file_title_drops_only_upload_noise() {
    for (title, expected) in [
        ("Song (Official Video)", "Song"),
        ("Song (Official Music Video) [HD]", "Song"),
        ("Song [Official Audio]", "Song"),
        ("Song (Lyric Video)", "Song"),
        ("Song (Live) (Official Video)", "Song (Live)"),
    ] {
        assert_eq!(PathHelper::file_title(title, "Artist"), expected, "{title}");
    }
}

#[test]
fn file_title_drops_a_redundant_artist_prefix() {
    assert_eq!(
        PathHelper::file_title("Massive Attack - Teardrop", "Massive Attack"),
        "Teardrop"
    );
}

/// Mezzanine's "(Exchange)" is the whole title, not an annotation.
#[test]
fn file_title_title_that_is_only_a_bracket_is_kept() {
    for title in ["(Exchange)", "(Official Video)"] {
        assert_eq!(PathHelper::file_title(title, "Massive Attack"), title);
    }
}

// ---- PathHelperLayoutTests ----

const LAYOUT_ROOT: &str = "/music";

#[test]
fn flat_puts_everything_in_one_directory() {
    let path = PathHelper::build_layout_path(
        FolderStructure::Flat,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk - Digital Love.flac");
}

#[test]
fn by_artist_uses_an_artist_folder_and_no_album_folder() {
    // The point of the layout: a library built a track at a time gets one folder per
    // artist instead of one folder per single, or one directory of thousands of files.
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love.flac");
}

#[test]
fn organized_keeps_the_album_folder_and_track_number() {
    let path = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Discovery/03 - Digital Love.flac");
}

#[test]
fn organized_no_album_falls_back_to_the_title_as_the_folder() {
    // Reproduces the pre-album shape for a genuine single, and the rule lives in the
    // resolver so every caller gets it rather than each one remembering.
    let path = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "Daft Punk",
        "",
        "Digital Love",
        None,
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love/Digital Love.flac");
}

#[test]
fn by_artist_no_album_is_unaffected() {
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "Daft Punk",
        "",
        "Digital Love",
        None,
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love.flac");
}

#[test]
fn empty_extension_is_honoured() {
    // The YouTube path passes no extension because the shim appends .mp3 itself.
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        "",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love");
}

/// `UnknownLayout_ThrowsRatherThanSilentlyPickingOne`: a C# enum can hold 999, a Rust one
/// cannot, and `build_layout_path`'s match has no default arm, so a new layout fails to
/// compile there. What is left to check is that every layout resolves to its own shape.
#[test]
fn unknown_layout_throws_rather_than_silently_picking_one() {
    let shapes: Vec<String> = [
        FolderStructure::Organized,
        FolderStructure::Flat,
        FolderStructure::ByArtist,
    ]
    .into_iter()
    .map(|layout| PathHelper::build_layout_path(layout, LAYOUT_ROOT, "A", "B", "C", Some(1), ".flac"))
    .collect();
    assert_eq!(
        shapes,
        ["/music/A/B/01 - C.flac", "/music/A - C.flac", "/music/A/C.flac"]
    );
}

// ---- WindowsDrivePathDetectionTests (SoulseekResolveRetryTests.cs): a Windows drive-letter
// path configured inside a Linux container is silently created as a literal directory name; the
// detector behind the startup warning must catch that shape and nothing else. ----

#[test]
fn drive_paths_are_detected() {
    for path in [r"E:\Media\Music", "E:/Media/Music", r"c:\music"] {
        assert!(PathHelper::looks_like_windows_drive_path(Some(path)), "{path}");
    }
}

#[test]
fn non_drive_paths_are_not() {
    for path in [
        Some("/music"),
        Some("./downloads"),
        Some("music"),
        Some("E:"),
        Some(""),
        None,
    ] {
        assert!(!PathHelper::looks_like_windows_drive_path(path), "{path:?}");
    }
}

// ---- Rust-only: the .NET path and string rules the port reproduces ----

#[test]
fn sanitize_cuts_at_one_hundred_utf16_units_then_trims() {
    let long = "a".repeat(99) + " b";
    assert_eq!(PathHelper::sanitize_file_name(&long), "a".repeat(99));
    // An astral character straddling the cut is left out whole.
    let astral = "a".repeat(99) + "\u{1F600}";
    assert_eq!(PathHelper::sanitize_file_name(&astral), "a".repeat(99));
    let dots = "a".repeat(98) + "..x";
    assert_eq!(PathHelper::sanitize_folder_name(&dots), "a".repeat(98));
    assert_eq!(PathHelper::sanitize_folder_name(" ... "), "Unknown");
    assert_eq!(PathHelper::sanitize_file_name("  "), "Unknown");
    assert_eq!(PathHelper::sanitize_file_name(" A/B "), "A_B");
}

#[test]
fn negative_track_numbers_keep_two_digits_after_the_sign() {
    assert!(PathHelper::build_track_path("r", "A", "B", "C", Some(-1), ".mp3").ends_with("/-01 - C.mp3"));
}

#[test]
fn resolve_unique_path_counts_up_past_existing_files() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let base = format!("{}/Song.flac", dir.path().display());
    assert_eq!(PathHelper::resolve_unique_path(&base), base);

    std::fs::write(&base, b"x").expect("write");
    let first = format!("{}/Song (1).flac", dir.path().display());
    assert_eq!(PathHelper::resolve_unique_path(&base), first);

    std::fs::write(&first, b"x").expect("write");
    assert_eq!(
        PathHelper::resolve_unique_path(&base),
        format!("{}/Song (2).flac", dir.path().display())
    );

    let bare = format!("{}/README", dir.path().display());
    std::fs::write(&bare, b"x").expect("write");
    assert_eq!(
        PathHelper::resolve_unique_path(&bare),
        format!("{}/README (1)", dir.path().display())
    );
}

#[test]
fn cache_path_is_under_the_temp_directory() {
    assert!(PathHelper::get_cache_path().ends_with("/octo-cache"));
}

#[test]
fn combine_follows_path_combine() {
    assert_eq!(combine("a/", "b"), "a/b");
    assert_eq!(combine("", "b"), "b");
    assert_eq!(combine("a", "/b"), "/b");
    assert_eq!(combine("a", ""), "a");
    assert_eq!(extension_of("/x/a.b.flac"), ".flac");
    assert_eq!(extension_of("/x.d/a"), "");
    assert_eq!(extension_of("a."), "");
    assert_eq!(file_name_without_extension("/x/a.b.flac"), "a.b");
    assert_eq!(directory_name("/a.flac"), "/");
    assert_eq!(directory_name("a.flac"), "");
}
