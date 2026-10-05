//! `ReleaseDistanceTests.cs`: each key's penalty function and the accumulator behind the release
//! chooser. The numbers here are the contract the calibration fixtures in ReleaseChooserTests
//! are computed from.

use super::*;

fn no_markers() -> BTreeSet<String> {
    BTreeSet::new()
}

fn markers(markers: &[&str]) -> BTreeSet<String> {
    markers.iter().map(|m| m.to_string()).collect()
}

fn types(types: &[&str]) -> Vec<String> {
    types.iter().map(|t| t.to_string()).collect()
}

/// `Assert.Equal(expected, actual, precision)`: both rounded to `digits` decimals.
fn assert_near(actual: f64, expected: f64, digits: u32, case: &str) {
    assert_eq!(
        crate::common::dotnet::round(actual, digits),
        crate::common::dotnet::round(expected, digits),
        "{case}: {actual}"
    );
}

// ---- title ---------------------------------------------------------------------------

#[test]
fn title_penalty_plain_request() {
    for (requested, candidate, expected) in [
        ("Teardrop", "Teardrop", 0.0),
        ("No Surprises (Official Video)", "No Surprises", 0.0),
        ("$UICIDE", "Suicide", 0.1),
        (
            "Bzrp Music Sessions #56",
            "Rauw Alejandro: Bzrp Music Sessions, Vol. 56",
            0.3,
        ),
        ("Teardrop", "Angel", 1.0),
        ("Teardrop", "Teardrop (Mad Professor mix)", 1.0),
    ] {
        assert_near(
            ReleaseDistance::title_penalty(requested, candidate, &no_markers(), &[]),
            expected,
            3,
            &format!("{requested} / {candidate}"),
        );
    }
}

/// A live take asked for and a title that does not say so is 0.8, unless the release's
/// kind says it is a live album, which is where the music database writes it.
#[test]
fn title_penalty_requested_marker_missing_is_covered_by_the_release_kind() {
    let cases = [
        ("Glory Box (Live)", "Glory Box", "live", types(&[]), 0.8),
        ("Glory Box (Live)", "Glory Box", "live", types(&["Live"]), 0.0),
        ("Song (Remix)", "Song", "remix", types(&[]), 0.8),
        ("Song (Remix)", "Song", "remix", types(&["Remix"]), 0.0),
    ];
    for (requested, candidate, marker, secondary, expected) in cases {
        assert_near(
            ReleaseDistance::title_penalty(requested, candidate, &markers(&[marker]), &secondary),
            expected,
            3,
            &format!("{requested} / {candidate} / {secondary:?}"),
        );
    }
}

#[test]
fn title_penalty_empty_title_is_the_whole_penalty() {
    assert_near(
        ReleaseDistance::title_penalty("Teardrop", "", &no_markers(), &[]),
        1.0,
        3,
        "empty",
    );
}

// ---- artist --------------------------------------------------------------------------

#[test]
fn artist_penalty_four_verdicts() {
    for (requested, credit, expected) in [
        ("Massive Attack", "Massive Attack feat. Elizabeth Fraser", 0.0),
        ("Bjork", "Björk", 0.0),
        ("Bizarrap, Rauw Alejandro", "Bizarrap & Rauw Alejandro", 0.0),
        ("Bizarrap, Duki", "Bizarrap & Rauw Alejandro", 1.0),
        ("", "Massive Attack", 0.5),
        ("Nirvana", "Foo Fighters", 1.0),
    ] {
        assert_near(
            ReleaseDistance::artist_penalty(requested, credit, &[]),
            expected,
            3,
            &format!("{requested} / {credit}"),
        );
    }
}

#[test]
fn artist_penalty_listed_credits_are_used_instead_of_splitting_the_joined_credit() {
    assert_near(
        ReleaseDistance::artist_penalty(
            "Earth, Wind & Fire",
            "Earth, Wind & Fire",
            &types(&["Earth, Wind & Fire"]),
        ),
        0.0,
        3,
        "EWF",
    );
}

// ---- length --------------------------------------------------------------------------

#[test]
fn length_penalty_five_seconds_grace_thirty_to_the_whole() {
    for (file, candidate, expected) in [
        (330, 330, 0.0),
        (330, 335, 0.0),
        (330, 350, 0.5),
        (330, 370, 1.0),
        (330, 290, 1.0),
    ] {
        assert_near(
            ReleaseDistance::length_penalty(file, candidate),
            expected,
            3,
            &format!("{file} / {candidate}"),
        );
    }
}

// ---- album ---------------------------------------------------------------------------

#[test]
fn album_penalty_editions_the_request_never_named_cost_a_little() {
    for (requested, candidate, expected) in [
        ("Mezzanine", "Mezzanine", 0.0),
        ("Discovery", "Discovery (Deluxe Edition)", 0.3),
        ("Nevermind", "Nevermind - 20th Anniversary", 0.3),
        ("Discovery", "Musique Vol. 1 (1993-2005)", 1.0),
    ] {
        assert_near(
            ReleaseDistance::album_penalty(requested, candidate),
            expected,
            3,
            &format!("{requested} / {candidate}"),
        );
    }
}

// ---- type ----------------------------------------------------------------------------

#[test]
fn type_penalty_plain_request() {
    let cases: [(Option<&str>, &[&str], f64); 11] = [
        (Some("Album"), &[], 0.0),
        (Some("Single"), &[], 0.2),
        (Some("EP"), &[], 0.2),
        (Some("Album"), &["Soundtrack"], 0.5),
        (Some("Album"), &["DJ-mix"], 0.8),
        (Some("Album"), &["Mixtape/Street"], 0.8),
        (Some("Album"), &["Compilation"], 1.0),
        (Some("Album"), &["Live"], 1.0),
        (Some("Album"), &["Remix"], 1.0),
        (None, &[], 0.5),
        (Some("Other"), &[], 0.5),
    ];
    for (primary, secondary, expected) in cases {
        assert_near(
            ReleaseDistance::type_penalty(primary, &types(secondary), &no_markers()),
            expected,
            3,
            &format!("{primary:?} / {secondary:?}"),
        );
    }
}

#[test]
fn type_penalty_live_request() {
    let cases: [(&str, &[&str], f64); 3] = [
        ("Album", &["Live"], 0.0),
        ("Album", &[], 0.6),
        ("Single", &[], 0.6),
    ];
    for (primary, secondary, expected) in cases {
        assert_near(
            ReleaseDistance::type_penalty(Some(primary), &types(secondary), &markers(&["live"])),
            expected,
            3,
            &format!("{primary} / {secondary:?}"),
        );
    }
}

#[test]
fn type_penalty_remix_request() {
    let cases: [(&str, &[&str], f64); 3] = [
        ("Album", &["Remix"], 0.0),
        ("Single", &[], 0.2),
        ("Album", &[], 0.5),
    ];
    for (primary, secondary, expected) in cases {
        assert_near(
            ReleaseDistance::type_penalty(Some(primary), &types(secondary), &markers(&["remix"])),
            expected,
            3,
            &format!("{primary} / {secondary:?}"),
        );
    }
}

// ---- original, status, source, year, country, barcode --------------------------------

#[test]
fn original_penalty_a_generation_later_is_the_whole() {
    for (group_first, earliest, expected) in [(1998, 1998, 0.0), (2017, 1997, 0.8), (2030, 1990, 1.0)] {
        assert_near(
            ReleaseDistance::original_penalty(group_first, earliest),
            expected,
            3,
            &format!("{group_first} / {earliest}"),
        );
    }
}

#[test]
fn status_penalty() {
    for (status, expected) in [
        ("Official", 0.0),
        ("Promotion", 0.5),
        ("Bootleg", 1.0),
        ("Pseudo-Release", 1.0),
        ("Something new", 0.25),
    ] {
        assert_near(ReleaseDistance::status_penalty(Some(status)), expected, 3, status);
    }
}

#[test]
fn source_penalty_trusts_the_fingerprint_most() {
    for (source, expected) in [
        (TagSource::Fingerprint, 0.0),
        (TagSource::Database, 0.1),
        (TagSource::Catalog, 0.25),
        (TagSource::FileTags, 0.5),
    ] {
        assert_near(
            ReleaseDistance::source_penalty(source),
            expected,
            3,
            source.name(),
        );
    }
}

#[test]
fn year_penalty_the_gap_as_a_share_of_the_candidates_age() {
    for (known, candidate, original, expected) in [
        (2011, 1991, Some(1991), 0.571),
        (2011, 2011, Some(1991), 0.0),
        (1991, 2011, Some(1991), 0.0),
        (1991, 2011, None, 1.0),
    ] {
        assert_near(
            ReleaseDistance::year_penalty(known, Some(candidate), original, 2026),
            expected,
            2,
            &format!("{known} / {candidate} / {original:?}"),
        );
    }
}

#[test]
fn country_penalty_position_in_the_list() {
    let preferred = types(&["US", "XW", "GB"]);
    for (country, expected) in [("US", 0.0), ("XW", 0.333), ("GB", 0.667), ("DE", 1.0)] {
        assert_near(
            ReleaseDistance::country_penalty(Some(country), &preferred),
            expected,
            2,
            country,
        );
    }
}

#[test]
fn barcode_penalty_twelve_and_thirteen_digit_forms_agree() {
    assert_eq!(
        ReleaseDistance::barcode_penalty(Some("0602475682233"), Some("602475682233")),
        Some(0.0)
    );
    assert_eq!(
        ReleaseDistance::barcode_penalty(Some("0602475682233"), Some("724384960629")),
        Some(1.0)
    );
    assert_eq!(
        ReleaseDistance::barcode_penalty(Some("not a code"), Some("724384960629")),
        None
    );
    assert_eq!(ReleaseDistance::barcode_penalty(None, Some("724384960629")), None);
}

// ---- the accumulator -----------------------------------------------------------------

#[test]
fn distance_is_the_weighted_sum_over_the_weights_that_applied() {
    let mut distance = Distance::new();
    distance.add("title", 1.0, 3.0);
    distance.add("artist", 0.0, 2.0);
    assert_near(distance.value(), 0.6, 3, "value");
}

#[test]
fn distance_a_key_added_twice_counts_its_weight_twice() {
    let mut distance = Distance::new();
    distance.add("a", 0.5, 2.0);
    distance.add("a", 0.5, 2.0);
    assert_near(distance.value(), 0.5, 3, "value");
    assert_eq!(distance.breakdown().len(), 2);
}

#[test]
fn distance_an_absent_key_changes_nothing_and_no_keys_at_all_is_the_whole() {
    let mut distance = Distance::new();
    distance.add("b", 0.0, 1.0);
    assert_near(distance.value(), 0.0, 3, "value");
    assert_eq!(distance.penalty_of("a"), None);
    assert_near(Distance::new().value(), 1.0, 3, "empty");
}

#[test]
fn distance_clamps_a_penalty_to_one() {
    let mut distance = Distance::new();
    distance.add("a", 7.0, 1.0);
    assert_near(distance.value(), 1.0, 3, "value");
}

// ---- Rust-only ----------------------------------------------------------------------

#[test]
fn barcode_forms_keep_digits_and_add_the_upc() {
    assert_eq!(
        barcode_forms(Some("0602475682233")),
        ["0602475682233", "602475682233"]
    );
    assert_eq!(barcode_forms(Some("7 24384 96062 9")), ["724384960629"]);
    assert!(barcode_forms(Some("1234567")).is_empty());
    assert!(barcode_forms(Some("123456789012345")).is_empty());
    assert_eq!(
        barcode_forms(Some("00000012345678")),
        ["00000012345678", "000012345678"]
    );
}
