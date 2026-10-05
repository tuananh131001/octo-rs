//! `GenreNormalizerTests.cs`. Downloads arrive with whatever the source called a genre. The
//! reporter's library had 316 genres for 1,900 tracks, and the mechanism was subtler than it
//! looks: genre was only ever written when non-empty and never cleared, so junk reached the
//! library through the ABSENCE of a write rather than a bad one.
//!
//! The two settings-only tests (`EffectiveMappings_*`, `EffectiveBlocklist_*`) were ported with
//! the settings, in `settings/genre.rs`.

use super::*;

fn preset_with(max: i32, on_empty: GenreEmptyBehavior) -> GenreSettings {
    GenreSettings {
        enabled: true,
        max_genres: max,
        on_empty,
        mappings: GenreSettings::broad_genre_preset(),
        ..Default::default()
    }
}

fn preset() -> GenreSettings {
    preset_with(1, GenreEmptyBehavior::Clear)
}

fn bare(rules: &[(&str, &str)]) -> GenreSettings {
    GenreSettings {
        enabled: true,
        max_genres: 1,
        mappings: rules
            .iter()
            .map(|(pattern, genre)| GenreMappingSettings {
                pattern: pattern.to_string(),
                genre: genre.to_string(),
                enabled: true,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn enabled(max: i32) -> GenreSettings {
    GenreSettings {
        enabled: true,
        max_genres: max,
        ..Default::default()
    }
}

/// GenreSettings { Enabled = true } with the shipped MaxGenres.
fn shipped() -> GenreSettings {
    GenreSettings {
        enabled: true,
        ..Default::default()
    }
}

fn normalize(raw: &[&str], settings: &GenreSettings) -> GenreNormalizationResult {
    GenreNormalizer::normalize(raw.iter().map(|value| Some(*value)), settings)
}

fn primary(raw: &str, settings: &GenreSettings) -> Option<String> {
    normalize(&[raw], settings).primary
}

fn plan(frame: &[&str], resolved: Option<&str>, settings: &GenreSettings) -> GenreTagPlan {
    GenreNormalizer::plan(frame, resolved, settings, None)
}

/// The exact strings from issue #41. This is the test that would have caught it.
#[test]
fn normalize_collapses_the_reporters_junk() {
    for (raw, expected) in [
        ("chicago rap", "Hip-Hop"),
        ("pop rap", "Hip-Hop"),
        ("trap latino", "Latin"),
        ("dance-pop", "Pop"),
        ("alternative rock", "Rock"),
    ] {
        assert_eq!(primary(raw, &preset()).as_deref(), Some(expected), "{raw}");
    }
}

#[test]
fn normalize_non_genres_and_years_are_dropped() {
    for raw in ["Music", "People & Blogs", "Gaming", "1998", "90s", "2020s"] {
        assert!(normalize(&[raw], &preset()).genres.is_empty(), "{raw}");
    }
}

/// Order is the entire semantics, which is why the dashboard control is an ordered
/// drag-and-drop list and not the unordered pinned-stations editor.
#[test]
fn normalize_applies_rules_in_order_first_match_wins() {
    assert_eq!(
        primary(
            "trap latino",
            &bare(&[("trap", "Hip-Hop"), ("trap latino", "Latin")])
        )
        .as_deref(),
        Some("Hip-Hop")
    );
    assert_eq!(
        primary(
            "trap latino",
            &bare(&[("trap latino", "Latin"), ("trap", "Hip-Hop")])
        )
        .as_deref(),
        Some("Latin")
    );
}

/// Splitting on '&' was the obvious implementation and it destroys two real genres.
#[test]
fn normalize_never_splits_on_ampersand() {
    for raw in ["R&B", "Drum & Bass"] {
        assert_eq!(normalize(&[raw], &enabled(5)).genres.len(), 1, "{raw}");
    }
}

#[test]
fn normalize_splits_the_usual_separators() {
    for raw in ["Rock; Pop", "Rock/Pop", "Rock, Pop"] {
        assert_eq!(normalize(&[raw], &enabled(5)).genres, ["Rock", "Pop"], "{raw}");
    }
}

/// An ID3v1 numeric genre sometimes leaks through a v2 frame as literal text.
#[test]
fn normalize_strips_id3v1_numeric_residue() {
    assert_eq!(primary("(17)Rock", &shipped()).as_deref(), Some("Rock"));
}

/// A blocklist entry means "this is not a genre at all", so it has to beat a substring
/// rule that would otherwise rescue it.
#[test]
fn normalize_blocklist_beats_mapping() {
    assert!(
        normalize(&["gaming"], &bare(&[("gam", "Games")]))
            .genres
            .is_empty()
    );
}

/// The key is already lowercased by the time the title-caser sees it, so the acronym guard
/// has to read the original text or EDM, IDM and UKG all come back as Edm, Idm and Ukg.
#[test]
fn normalize_unmapped_genres_are_title_cased_but_acronyms_survive() {
    for (raw, expected) in [
        ("EDM", "EDM"),
        ("IDM", "IDM"),
        ("indie rock", "Indie Rock"),
        ("shoegaze", "Shoegaze"),
    ] {
        assert_eq!(primary(raw, &shipped()).as_deref(), Some(expected), "{raw}");
    }
}

/// Found by a whole-library preview against a real 2,364-file library: the only proposed
/// change for one file was "Alternatif et Indé" -> "Alternatif Et Indé". No rule matched;
/// title-casing alone counted as a change, so the backfill would have rewritten the file
/// purely to capitalise "et". Tidying case is only worth a write when the source clearly
/// did not bother.
#[test]
fn normalize_unmapped_genre_that_is_already_capitalised_keeps_its_own_spelling() {
    for raw in ["Alternatif et Indé", "Psychedelic Rock", "Hip Hop"] {
        assert_eq!(primary(raw, &shipped()).as_deref(), Some(raw));
    }
}

/// A file whose genre only needs re-casing is left byte-identical.
#[test]
fn plan_only_difference_would_be_casing_is_not_a_change() {
    let plan = plan(&["Alternatif et Indé"], None, &enabled(5));

    assert_eq!(plan.action, GenreTagAction::Write);
    // The backfill compares this against the existing frame ordinally, so an identical
    // sequence means it writes nothing at all.
    assert_eq!(plan.genres, ["Alternatif et Indé"]);
}

#[test]
fn normalize_caps_at_max_genres_and_dedupes() {
    assert_eq!(
        normalize(&["Rock; Pop; Rock; Jazz"], &enabled(2)).genres,
        ["Rock", "Pop"]
    );
}

/// The whole point of the feature. Without a Clear, "People & Blogs" survives
/// normalisation and the library keeps it forever.
#[test]
fn plan_everything_normalises_away_clears() {
    let plan = plan(&["People & Blogs", "Music"], None, &preset());

    assert_eq!(plan.action, GenreTagAction::Clear);
    assert!(plan.genres.is_empty());
}

/// Writing an empty frame over an absent one is a rewrite with no benefit, and it dirties
/// a file the run should have left byte-identical.
#[test]
fn plan_absent_frame_and_nothing_resolved_leaves_the_file_alone() {
    assert_eq!(plan(&[], None, &preset()).action, GenreTagAction::None);
}

#[test]
fn plan_on_empty_unknown_writes_the_label_instead() {
    let plan = plan(&["Music"], None, &preset_with(1, GenreEmptyBehavior::Unknown));

    assert_eq!(plan.action, GenreTagAction::Write);
    assert_eq!(plan.primary.as_deref(), Some("Unknown"));
}

#[test]
fn plan_on_empty_leave_is_the_old_behaviour() {
    assert_eq!(
        plan(&["Music"], None, &preset_with(1, GenreEmptyBehavior::Leave)).action,
        GenreTagAction::None
    );
}

/// A resolved genre outranks a stranger's tag, but does not erase a usable one.
#[test]
fn plan_resolved_genre_leads_the_frames_own_values() {
    assert_eq!(
        plan(
            &["Trap"],
            Some("Rock"),
            &preset_with(2, GenreEmptyBehavior::Clear)
        )
        .primary
        .as_deref(),
        Some("Rock")
    );
}

/// The fallback earns a turn only when nothing in the file survived.
#[test]
fn plan_fallback_only_applies_when_nothing_survived() {
    // "Music" is blocklisted, so without the fallback this would clear. shoegaze maps to
    // Rock under the preset, which is the answer that proves the fallback was consulted.
    let with_fallback = GenreNormalizer::plan(&["Music"], None, &preset(), Some(&["shoegaze"][..]));
    assert_eq!(with_fallback.action, GenreTagAction::Write);
    assert_eq!(with_fallback.primary.as_deref(), Some("Rock"));

    let unused = GenreNormalizer::plan(&["Jazz"], None, &preset(), Some(&["shoegaze"][..]));
    assert_eq!(unused.primary.as_deref(), Some("Jazz"));
}

/// A resumed backfill re-processes files it already touched, so running the plan over its
/// own output has to be a no-change.
#[test]
fn plan_is_idempotent() {
    let first = plan(&["chicago rap"], None, &preset());
    let second = GenreNormalizer::plan(first.genres.as_slice(), None, &preset(), None);

    assert_eq!(first.genres, second.genres);
}

/// This is the test that enforces the two year rules agree. Two hand-rolled copies is how
/// one of them quietly stops dropping "2020s" when someone fixes the other. (The radio
/// kinship half of the C# test, `LastFmRadioStreamService.KinshipTags`, is checked beside
/// `kinship_tags` in `last_fm::last_fm_radio_stream_service`.)
#[test]
fn is_year_like_matches_what_the_radio_kinship_filter_also_drops() {
    for (tag, expected) in [
        ("1998", true),
        ("2026", true),
        ("80", true),
        ("90s", true),
        ("1990s", true),
        ("2020s", true),
        ("nu metal", false),
        ("4ad", false),
    ] {
        assert_eq!(GenreNormalizer::is_year_like(tag), expected, "{tag}");
    }
}

#[test]
fn is_year_like_empty_is_not_a_year() {
    assert!(!GenreNormalizer::is_year_like(""));
}

/// A Turkish-locale container turns "I" into a dotless "ı" through ToLower and ToTitleCase,
/// which silently stops "Indie" matching "indie". Rust has no process culture: the port is
/// invariant by construction, which this pins.
#[test]
fn normalize_is_culture_invariant() {
    assert_eq!(primary("indie", &shipped()).as_deref(), Some("Indie"));
    assert_eq!(primary("EDM", &shipped()).as_deref(), Some("EDM"));
}

/// Switching normalisation on, with nothing else configured, must not throw away accurate
/// genres. The default used to be one genre per track, so simply enabling the feature turned
/// "Cloud Rap, Emo, Hip Hop, Trap" into "Cloud Rap" with no mapping table involved at all.
/// Dropping junk is what this is for; minifying real tags is a choice the user has to make.
#[test]
fn defaults_keep_every_real_genre_on_a_well_tagged_file() {
    for (raw, expected) in [
        ("Cloud Rap, Emo, Hip Hop, Trap", 4),
        ("Psychedelic Rock, Neo-Psychedelia", 2),
        ("Electronic, Disco, Funk, Electro", 4),
    ] {
        let plan = plan(&[raw], None, &shipped());

        assert_eq!(plan.genres.len(), expected, "{raw}");
        assert_eq!(
            plan.genres,
            raw.split(',').map(str::trim).collect::<Vec<_>>(),
            "{raw}"
        );
    }
}

/// The junk-dropping half still works with the defaults; that is the part nobody
/// has to opt into.
#[test]
fn defaults_still_drop_junk_and_duplicates() {
    assert_eq!(
        plan(&["Rock, Music, 1998, Rock, People & Blogs"], None, &shipped()).genres,
        ["Rock"]
    );
}

#[test]
fn broad_genre_preset_collapses_real_world_junk_to_a_short_list() {
    let settings = preset();
    let fixture = [
        "chicago rap",
        "pop rap",
        "trap latino",
        "dance-pop",
        "k-pop",
        "alternative rock",
        "neo-soul",
        "drum and bass",
        "synthwave",
        "grindcore",
        "bluegrass",
        "bebop",
        "shoegaze",
        "dancehall",
        "trance",
        "baroque",
        "singer-songwriter",
        "phonk",
    ];

    let mut produced = IgnoreCaseSet::new();
    let mut found = 0;
    for raw in fixture {
        if let Some(genre) = primary(raw, &settings) {
            found += 1;
            produced.insert(genre);
        }
    }

    assert_eq!(found, fixture.len());
    assert!((1..=14).contains(&produced.len()), "{} genres", produced.len());
}

// ---- Rust-only: TextInfo.ToTitleCase, as .NET 9 answered for these -----------------------

#[test]
fn title_case_follows_dotnet_word_rules() {
    for (text, expected) in [
        ("indie rock", "Indie Rock"),
        ("4ad", "4Ad"),
        ("hip-hop/rap", "Hip-Hop/Rap"),
        ("r&b", "R&B"),
        ("drum'n'bass", "Drum'n'bass"),
        ("dž test", "Dž Test"),
        ("ǆ", "ǅ"),
        ("𝐚bc def", "𝐚bc Def"),
        ("日本 rock", "日本 Rock"),
        ("ʰello", "ʰello"),
        ("a1b c", "A1b C"),
        ("x.y z", "X.Y Z"),
        ("ABC def", "ABC Def"),
        ("aBC", "Abc"),
        ("post-rock", "Post-Rock"),
    ] {
        assert_eq!(to_title_case(text), expected, "{text}");
    }
}

#[test]
fn exact_rules_and_disabled_rules() {
    let mut settings = bare(&[("rap", "Hip-Hop"), ("rock", "Rock!")]);
    settings.mappings[0].match_mode = GenreMatchMode::Exact;
    settings.mappings[1].enabled = false;
    assert_eq!(primary("pop rap", &settings).as_deref(), Some("Pop Rap"));
    assert_eq!(primary("RAP", &settings).as_deref(), Some("Hip-Hop"));
    assert_eq!(primary("rock", &settings).as_deref(), Some("Rock"));
    // A rule for the whole compound fires before the split; the rule is named.
    let compound = bare(&[("hip-hop/rap", "Hip-Hop")]);
    let result = normalize(&["Hip-Hop/Rap"], &compound);
    assert_eq!(result.genres, ["Hip-Hop"]);
    assert_eq!(result.matched_rule.as_deref(), Some("hip-hop/rap -> Hip-Hop"));
}
