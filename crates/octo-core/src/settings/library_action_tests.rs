//! LibraryActionSettingsTests.cs, plus the settings-only half of DuplicateTests.cs
//! (DuplicateSettingsTests).
//!
//! Every part of library actions is configurable, which means the sanitising has to be too.
//! The defaults are the cautious reading of every choice, because the feature removes files
//! Octo did not create.

use super::*;

fn def(action: LibraryAction, name: &str, enabled: bool, rating: Option<i32>) -> LibraryActionDefinition {
    LibraryActionDefinition {
        action,
        name: name.to_string(),
        enabled,
        rating,
    }
}

fn find(actions: &[LibraryActionDefinition], action: LibraryAction) -> &LibraryActionDefinition {
    let matching: Vec<_> = actions.iter().filter(|a| a.action == action).collect();
    assert_eq!(matching.len(), 1, "exactly one {action:?}");
    matching[0]
}

#[test]
fn defaults_are_the_cautious_reading_of_every_choice() {
    let settings = LibraryActionSettings::default();

    assert!(!settings.enabled);
    assert!(!settings.ratings_enabled);
    assert!(settings.dry_run);
    assert!(settings.allowed_users.is_empty());
}

/// Fail-open on a feature that removes files is not a defensible default, so an empty
/// allowlist has to mean nobody rather than everybody.
#[test]
fn is_allowed_empty_list_is_false_for_everyone() {
    let settings = LibraryActionSettings::default();

    assert!(!settings.is_allowed(Some("alice")));
    assert!(!settings.is_allowed(Some("")));
    assert!(!settings.is_allowed(None));
}

#[test]
fn is_allowed_matches_navidrome_usernames_case_insensitively() {
    let settings = LibraryActionSettings {
        allowed_users: vec!["Alice".into()],
        ..Default::default()
    };
    for (username, expected) in [
        ("alice", true),
        ("ALICE", true),
        (" alice ", true),
        ("bob", false),
    ] {
        assert_eq!(settings.is_allowed(Some(username)), expected, "{username:?}");
    }
}

/// Disabling one action must not delete the names chosen for the others, and a config
/// naming only some of them must not leave the rest undefined.
#[test]
fn effective_actions_back_fills_every_action_and_keeps_configured_names() {
    let settings = LibraryActionSettings {
        actions: vec![def(LibraryAction::Delete, "Bin it", true, Some(1))],
        ..Default::default()
    };

    let actions = settings.effective_actions();

    assert_eq!(actions.len(), 5);
    let delete = find(&actions, LibraryAction::Delete);
    assert_eq!(delete.name, "Bin it");
    assert!(delete.enabled);
    // The rest come back with their defaults, disabled.
    assert!(
        actions
            .iter()
            .filter(|a| a.action != LibraryAction::Delete)
            .all(|a| !a.enabled)
    );
}

/// Two playlists with the same effective title are indistinguishable to the sweep, and one
/// of them would apply the wrong action.
#[test]
fn effective_actions_drops_a_duplicate_effective_title() {
    let settings = LibraryActionSettings {
        actions: vec![
            def(LibraryAction::Delete, "Fix it", false, None),
            def(LibraryAction::WrongSong, "Fix it", false, None),
        ],
        ..Default::default()
    };

    assert_eq!(settings.effective_actions().len(), 4);
}

#[test]
fn effective_actions_blank_or_overlong_name_falls_back_to_the_built_in() {
    let settings = LibraryActionSettings {
        actions: vec![
            def(LibraryAction::Delete, "   ", false, None),
            def(LibraryAction::WrongSong, &"x".repeat(200), false, None),
        ],
        ..Default::default()
    };

    let actions = settings.effective_actions();
    assert_eq!(find(&actions, LibraryAction::Delete).name, "Delete");
    assert_eq!(find(&actions, LibraryAction::WrongSong).name, "Wrong song");
}

/// A rating can only mean one thing, so a collision unmaps the later one.
#[test]
fn effective_actions_two_actions_on_the_same_rating_unmaps_the_second() {
    let settings = LibraryActionSettings {
        actions: vec![
            def(LibraryAction::Delete, "Delete", true, Some(1)),
            def(LibraryAction::WrongSong, "Wrong song", true, Some(1)),
        ],
        ..Default::default()
    };

    let actions = settings.effective_actions();
    assert_eq!(find(&actions, LibraryAction::Delete).rating, Some(1));
    assert_eq!(find(&actions, LibraryAction::WrongSong).rating, Some(0));
}

/// Five stars is Keep by default, which removes nothing, so the top of the scale is never
/// destructive and an enthusiastic rating cannot remove a file.
#[test]
fn action_for_rating_five_stars_means_keep() {
    let settings = LibraryActionSettings {
        actions: LibraryAction::ALL
            .iter()
            .map(|a| def(*a, "", true, None))
            .collect(),
        ..Default::default()
    };

    assert_eq!(
        settings.action_for_rating(5).map(|a| a.action),
        Some(LibraryAction::Keep)
    );
    assert_eq!(
        settings.action_for_rating(1).map(|a| a.action),
        Some(LibraryAction::Delete)
    );
}

/// Keep only answers Octo's questions, so it comes on by itself with Review. A choice the
/// operator made either way still wins.
#[test]
fn effective_actions_keep_comes_on_with_review_unless_configured() {
    fn keep_enabled(settings: &LibraryActionSettings) -> bool {
        find(&settings.effective_actions(), LibraryAction::Keep).enabled
    }

    assert!(!keep_enabled(&LibraryActionSettings::default()));
    assert!(keep_enabled(&LibraryActionSettings {
        review_enabled: true,
        ..Default::default()
    }));
    assert!(!keep_enabled(&LibraryActionSettings {
        review_enabled: true,
        actions: vec![def(LibraryAction::Keep, "", false, None)],
        ..Default::default()
    }));
    assert!(keep_enabled(&LibraryActionSettings {
        actions: vec![def(LibraryAction::Keep, "", true, None)],
        ..Default::default()
    }));
}

/// Auto keeps the behaviour ratings had before the setting existed until Review is on, and
/// then a star only counts on a track Octo asked about.
#[test]
fn effective_ratings_scope_auto_follows_notices() {
    assert_eq!(
        LibraryActionSettings::default().effective_ratings_scope(),
        LibraryRatingScope::Global
    );
    assert_eq!(
        LibraryActionSettings {
            review_enabled: true,
            ..Default::default()
        }
        .effective_ratings_scope(),
        LibraryRatingScope::NoticeOnly
    );
}

#[test]
fn effective_ratings_scope_explicit_wins() {
    for (configured, review, expected) in [
        (LibraryRatingScope::Global, true, LibraryRatingScope::Global),
        (
            LibraryRatingScope::NoticeOnly,
            false,
            LibraryRatingScope::NoticeOnly,
        ),
    ] {
        let settings = LibraryActionSettings {
            ratings_scope: configured,
            review_enabled: review,
            ..Default::default()
        };
        assert_eq!(
            settings.effective_ratings_scope(),
            expected,
            "{configured:?}/{review}"
        );
    }
}

#[test]
fn notice_title_uses_the_notice_prefix_and_falls_back_on_a_blank_name() {
    assert_eq!(
        LibraryActionSettings::default().notice_title(NoticeKind::Review),
        "▸ Review"
    );
    assert_eq!(
        LibraryActionSettings {
            notice_prefix: "? ".into(),
            review_playlist_name: " To check ".into(),
            ..Default::default()
        }
        .notice_title(NoticeKind::Review),
        "? To check"
    );
    assert_eq!(
        LibraryActionSettings {
            notice_prefix: "".into(),
            review_playlist_name: "  ".into(),
            ..Default::default()
        }
        .notice_title(NoticeKind::Review),
        "Review"
    );
}

#[test]
fn effective_notice_max_tracks_is_clamped() {
    for (configured, expected) in [(0, 1), (100, 100), (100000, 500)] {
        let settings = LibraryActionSettings {
            notice_max_tracks: configured,
            ..Default::default()
        };
        assert_eq!(settings.effective_notice_max_tracks(), expected, "{configured}");
    }
}

#[test]
fn action_for_rating_out_of_range_is_nothing() {
    for rating in [0, 6, -1] {
        assert!(
            LibraryActionSettings::default()
                .action_for_rating(rating)
                .is_none(),
            "{rating}"
        );
    }
}

#[test]
fn action_for_rating_disabled_action_is_not_triggered() {
    let settings = LibraryActionSettings {
        actions: vec![def(LibraryAction::Delete, "", false, Some(1))],
        ..Default::default()
    };

    assert!(settings.action_for_rating(1).is_none());
}

/// A directory traversal must not be typeable into a settings field.
#[test]
fn effective_quarantine_directory_is_one_safe_segment() {
    for (configured, expected) in [
        ("../../etc", ".octo-trash"),
        ("..", ".octo-trash"),
        (".", ".octo-trash"),
        ("", ".octo-trash"),
        ("trash/nested", "trash"),
        ("/.octo-trash/", ".octo-trash"),
        ("my-bin", "my-bin"),
    ] {
        let settings = LibraryActionSettings {
            quarantine_directory: configured.into(),
            ..Default::default()
        };
        assert_eq!(
            settings.effective_quarantine_directory(),
            expected,
            "{configured:?}"
        );
    }
}

/// 0 is a real choice meaning "never sweep", so it is not clamped upward.
#[test]
fn effective_quarantine_retention_days_treats_zero_as_forever() {
    for (configured, expected) in [(0, 0), (-5, 0), (30, 30), (99999, 3650)] {
        let settings = LibraryActionSettings {
            quarantine_retention_days: configured,
            ..Default::default()
        };
        assert_eq!(
            settings.effective_quarantine_retention_days(),
            expected,
            "{configured}"
        );
    }
}

#[test]
fn effective_poll_interval_is_clamped() {
    for (configured, expected_seconds) in [(1, 15), (60, 60), (99999, 3600)] {
        let settings = LibraryActionSettings {
            poll_interval_seconds: configured,
            ..Default::default()
        };
        assert_eq!(
            settings.effective_poll_interval().as_secs(),
            expected_seconds,
            "{configured}"
        );
    }
}

#[test]
fn playlist_title_uses_the_configured_prefix_and_tolerates_none() {
    let with_prefix = LibraryActionSettings {
        playlist_prefix: ">> ".into(),
        ..Default::default()
    };
    let definition = with_prefix
        .effective_actions()
        .into_iter()
        .find(|a| a.action == LibraryAction::Delete);
    assert_eq!(
        with_prefix.playlist_title(&definition.expect("delete")),
        ">> Delete"
    );

    let none = LibraryActionSettings {
        playlist_prefix: "".into(),
        ..Default::default()
    };
    let definition = none
        .effective_actions()
        .into_iter()
        .find(|a| a.action == LibraryAction::Delete);
    assert_eq!(none.playlist_title(&definition.expect("delete")), "Delete");
}

// ---- DuplicateTests.cs: DuplicateSettingsTests ---------------------------------------------

#[test]
fn duplicates_alone_turns_on_notices_keep_and_the_notice_only_scope() {
    let settings = LibraryActionSettings {
        duplicates_enabled: true,
        ..Default::default()
    };

    assert!(settings.notices_enabled());
    assert_eq!(settings.enabled_notice_kinds(), [NoticeKind::Duplicates]);
    assert_eq!(settings.effective_ratings_scope(), LibraryRatingScope::NoticeOnly);
    assert!(find(&settings.effective_actions(), LibraryAction::Keep).enabled);
    assert_eq!(settings.notice_title(NoticeKind::Duplicates), "▸ Duplicates");
    assert_eq!(
        LibraryActionSettings {
            duplicates_playlist_name: " ".into(),
            ..Default::default()
        }
        .notice_title(NoticeKind::Duplicates),
        "▸ Duplicates"
    );
}

#[test]
fn effective_duplicates_scan_interval_is_clamped() {
    for (hours, expected) in [(0, 1), (24, 24), (10000, 168)] {
        let settings = LibraryActionSettings {
            duplicates_scan_hours: hours,
            ..Default::default()
        };
        assert_eq!(
            settings.effective_duplicates_scan_interval().as_secs() / 3600,
            expected,
            "{hours}"
        );
    }
}

// ---- Beyond the C# tests ---------------------------------------------------------------------

#[test]
fn mapped_ratings_lists_enabled_non_zero_ratings_in_order() {
    let settings = LibraryActionSettings {
        actions: vec![
            def(LibraryAction::Keep, "", true, None),
            def(LibraryAction::Delete, "", true, Some(0)),
            def(LibraryAction::WrongSong, "", true, None),
        ],
        ..Default::default()
    };
    assert_eq!(settings.mapped_ratings(), [2, 5]);
}

#[test]
fn library_action_indexes_match_the_journal_numbers() {
    assert_eq!(LibraryAction::Keep.as_index(), 4);
    assert_eq!(LibraryAction::from_index(3), Some(LibraryAction::BetterQuality));
    assert_eq!(LibraryAction::from_index(5), None);
    assert_eq!(LibraryAction::from_index(-1), None);
}
