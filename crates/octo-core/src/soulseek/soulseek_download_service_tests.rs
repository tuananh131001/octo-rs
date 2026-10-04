//! Port of `octo.Tests/SoulseekCandidateMatchingTests.cs` (its extension tests are in
//! `soulseek_client`'s), the Soulseek cases SongIdentityTests and LiveVersionTests deferred to
//! here, and AlbumFolderTests' `TheAlbumSearchWords` and `TheFolderOfAFileIsWhatThePeerNamesIt`.
//!
//! Which peer file gets accepted decides what audio ends up in the library, and a wrong
//! choice is invisible: the tagger stamps the correct title, track number and cover art
//! onto whatever arrived, so the library looks right and plays wrong.
//!
//! Every filename below is real, taken from a live Soulseek walk of Massive Attack's
//! Mezzanine that silently pulled four tracks off a Mad Professor remix album.

use super::*;

fn matches(filename: &str, title: &str) -> bool {
    filename_plausibly_matches_title(filename, title, false)
}

fn matches_phrase(filename: &str, title: &str) -> bool {
    filename_plausibly_matches_title(filename, title, true)
}

fn plausible(candidate: Option<i32>, expected: Option<i32>) -> bool {
    duration_plausible(candidate, expected, false)
}

// ---- filename matching ----------------------------------------------------

/// A folder name must not satisfy a track title. Matching used to run against the
/// whole path, so any file inside a "Mezzanine (1998)" directory answered a search
/// for the track "Mezzanine".
#[test]
fn folder_name_does_not_satisfy_a_track_title() {
    let in_mezzanine_folder = r"music\Massive Attack\Mezzanine (1998)\03 - Teardrop.flac";

    assert!(!matches(in_mezzanine_folder, "Mezzanine"));
    assert!(matches(in_mezzanine_folder, "Teardrop"));
}

/// The filename check alone CANNOT catch every case, and pretending otherwise would
/// be the wrong lesson. This peer encodes artist, album and title into one flat
/// filename, so the album name "Mezzanine" sits in the leaf and the name check passes
/// a file that is actually a different song. Duration and the variant marker are what
/// reject it, which is why all three layers exist.
#[test]
fn flat_filenames_need_the_other_two_signals() {
    let wrong = "Massive Attack_Massive Attack V Mad Professor Part II (Mezzanine Remix Tapes '98)_08_Group Four (Security Forces Dub).flac";

    // The name check cannot tell: "mezzanine" really is in the leaf.
    assert!(matches(wrong, "Mezzanine"));

    // These do. "Mezzanine" is 354s and this file is 494s.
    assert!(!plausible(Some(494), Some(354)));
    assert!(variant_penalty(wrong, "Mezzanine") > 0);
}

#[test]
fn real_track_filenames_still_match() {
    assert!(matches(
        r"music\Massive Attack\Mezzanine (1998)\09 - Mezzanine.flac",
        "Mezzanine"
    ));
    assert!(matches(
        "Massive Attack_Collected_01-05_Inertia Creeps.flac",
        "Inertia Creeps"
    ));
    assert!(matches(
        "Massive-Attack-Mezzanine-06-Dissolved-Girl.flac",
        "Dissolved Girl"
    ));
    assert!(matches("07 Massive Attack - Man Next Door.flac", "Man Next Door"));
}

/// Any-one-token matching let "Group Four" be satisfied by unrelated files.
#[test]
fn every_significant_token_must_be_present() {
    assert!(!matches("Frankie Valli - The Four Seasons.flac", "Group Four"));
    assert!(matches("10 - Group Four.flac", "Group Four"));
}

/// Short titles must not be filtered away by the >=3 char token rule.
#[test]
fn short_titles_are_not_over_filtered() {
    assert!(matches("Kendrick Lamar - DNA..flac", "DNA."));
}

// ---- title-only fallback strictness ---------------------------------------

/// The real file that answered a star for Jason Aldean's "The Truth" once the
/// artist dropped out of the query: every token of the title sits somewhere in the
/// name, so scattered-token matching passed a 136 MB dungeon-synth track. The
/// phrase rule is what rejects it, and the loose assertion documents why the phrase
/// rule exists rather than being a bug in the token rule.
#[test]
fn scattered_title_tokens_do_not_satisfy_the_title_only_fallback() {
    let wrong = "UNSHEATHED GLORY - Finale - The Greataxe of Shining Truth.flac";

    assert!(matches(wrong, "The Truth"));
    assert!(!matches_phrase(wrong, "The Truth"));
}

#[test]
fn real_filename_shapes_still_pass_the_phrase_rule() {
    assert!(matches_phrase("Jason Aldean - The Truth.flac", "The Truth"));
    assert!(matches_phrase("03 - The Truth.flac", "The Truth"));
    assert!(matches_phrase(
        r"music\Massive Attack\Mezzanine (1998)\09 - Mezzanine.flac",
        "Mezzanine"
    ));
    assert!(matches_phrase(
        "Massive-Attack-Mezzanine-06-Dissolved-Girl.flac",
        "Dissolved Girl"
    ));
}

/// Filenames routinely drop a leading article, and that is not a mismatch.
#[test]
fn a_leading_article_may_drop_from_the_filename() {
    assert!(matches_phrase("Jason Aldean - Truth.flac", "The Truth"));
}

/// Dotted acronyms space-normalize into single letters no filename spells out, so
/// the compact form is accepted — but the anything-goes pass that zero significant
/// tokens used to grant is exactly what the fallback cannot afford.
#[test]
fn dotted_acronyms_match_compact_but_no_longer_match_anything() {
    assert!(matches_phrase("MIA.flac", "M.I.A."));
    assert!(!matches_phrase("anything at all.flac", "M.I.A."));
}

/// The incident's second gate: the wrong file advertised no length, and unknown
/// used to pass unconditionally. On the fallback, a known catalog length makes an
/// unadvertised one disqualifying — rejection after the download still costs the
/// full transfer, and this candidate class is where the 136 MB one came from.
#[test]
fn unknown_length_loses_its_free_pass_on_the_title_only_fallback() {
    assert!(!duration_plausible(None, Some(245), true));
    assert!(!duration_plausible(Some(0), Some(245), true));

    assert!(plausible(None, Some(245)));
    assert!(duration_plausible(Some(243), Some(245), true));
    assert!(duration_plausible(None, None, true));
    assert!(duration_plausible(Some(245), None, true));
}

// ---- duration -------------------------------------------------------------

#[test]
fn wildly_wrong_length_is_rejected() {
    // "Mezzanine" is 354s; the file that arrived was 494s.
    assert!(!plausible(Some(494), Some(354)));
    // "Exchange" is 251s; a dub mix of 344s arrived.
    assert!(!plausible(Some(344), Some(251)));
}

#[test]
fn mastering_drift_is_accepted() {
    // Real spread measured across the album's correctly-matched tracks.
    assert!(plausible(Some(380), Some(378)));
    assert!(plausible(Some(331), Some(327)));
    assert!(plausible(Some(299), Some(299)));
}

/// The near misses that a looser tolerance let through: an "(Angel Dust)" dub 12s off
/// and an "(Floating on Dubwise)" dub 11s off, against correct tracks that never
/// drifted past 4s.
#[test]
fn near_miss_dub_mixes_are_rejected() {
    assert!(!plausible(Some(366), Some(378)));
    assert!(!plausible(Some(367), Some(356)));
}

/// Neither of these contains a keyword a marker list would catch. What gives them away
/// is that they are bracketed additions the requested title never asked for.
#[test]
fn unrequested_bracketed_additions_are_penalised() {
    assert!(
        variant_penalty(
            "2-02 Massive Attack & Mad Professor - Angel (Angel Dust).flac",
            "Angel"
        ) > 0
    );
    assert!(
        variant_penalty(
            "Massive Attack - Mezzanine - 04 - Inertia Creeps (Floating on Dubwise).flac",
            "Inertia Creeps"
        ) > 0
    );
}

/// Year and format tags are how peers label a good rip, not a different take.
#[test]
fn year_and_format_tags_are_not_treated_as_variants() {
    assert_eq!(variant_penalty("Angel (1998).flac", "Angel"), 0);
    assert_eq!(variant_penalty("Angel [FLAC].flac", "Angel"), 0);
}

#[test]
fn unknown_length_is_not_treated_as_evidence() {
    assert!(plausible(None, Some(354)));
    assert!(plausible(Some(354), None));
    assert!(plausible(Some(0), Some(354)));
}

// ---- variant markers ------------------------------------------------------

/// The case duration cannot catch: "Group Four (Security Forces dub)" runs 495s
/// against the album version's 493s, so only the name gives it away.
#[test]
fn unrequested_variants_sort_below_plain_matches() {
    let dub = variant_penalty(
        "2-08 Massive Attack & Mad Professor - Group Four (Security Forces dub).flac",
        "Group Four",
    );
    let plain = variant_penalty("10 Massive Attack - Group Four.flac", "Group Four");

    assert!(dub > plain, "a dub mix must rank below the plain album version");
    assert_eq!(plain, 0);
}

/// A remix that was actually asked for must not be penalised.
#[test]
fn requested_variants_are_not_penalised() {
    assert_eq!(
        variant_penalty(
            "05 - Teardrop (Mazaruni Dub One).flac",
            "Teardrop (Mazaruni Dub One)"
        ),
        0
    );
}

/// Word boundaries, so "Oliver" or "delivery" is not read as "live".
#[test]
fn markers_match_whole_words_only() {
    assert_eq!(
        variant_penalty("Oliver Nelson - Stolen Moments.flac", "Stolen Moments"),
        0
    );
}

// ---- roman numerals -------------------------------------------------------

/// The >=3 char token rule deleted the only thing separating these two titles, so
/// either file satisfied a request for the other and a two-part suite arrived as two
/// copies of the same part.
#[test]
fn roman_numeral_parts_are_not_interchangeable() {
    assert!(!matches("02 - Trilogy II.flac", "Trilogy I"));
    assert!(!matches("01 - Trilogy I.flac", "Trilogy II"));
    assert!(matches("01 - Trilogy I.flac", "Trilogy I"));
}

/// Word boundaries in both directions: "I" must not find itself inside "II", and "V"
/// must not find itself inside "IV".
#[test]
fn roman_numerals_match_whole_words_only() {
    assert!(!matches("Trilogy IV.flac", "Trilogy V"));
    assert!(matches("Trilogy IV.flac", "Trilogy IV"));
}

/// The guard is anchored to the end of the title, so ordinary titles carrying a
/// stray "I" or a trailing letter are untouched by it.
#[test]
fn ordinary_titles_are_unaffected_by_the_roman_guard() {
    assert!(matches("Kendrick Lamar - DNA..flac", "DNA."));
    assert!(matches(
        r"music\Massive Attack\Mezzanine (1998)\09 - Mezzanine.flac",
        "Mezzanine"
    ));
    assert!(matches("10 - Group Four.flac", "Group Four"));
}

#[test]
fn an_album_query_is_added_last_only_when_the_album_says_something() {
    let planned = planned_queries("Teardrop", "Massive Attack", Some("Mezzanine"), Some(330));
    let last = planned.last().expect("queries");
    assert_eq!(
        (last.0.text(), last.1),
        ("Massive Attack Mezzanine".to_string(), true)
    );
    assert!(!planned[0].1);
    assert!(
        !planned
            .iter()
            .any(|(q, _)| q.text().to_lowercase().contains("flac"))
    );
    let album = |t: &str, a: &str, al: &str, d: Option<i32>| album_query(Some(t), Some(a), Some(al), d);
    assert_eq!(album("Teardrop", "Massive Attack", "Mezzanine", None), None);
    assert_eq!(album("Teardrop", "Massive Attack", "Teardrop", Some(330)), None);
    assert_eq!(album("Hello", "Adele", "Hello - Single", Some(295)), None);
    assert_eq!(album("Song", "Artist", "[Unknown Album]", Some(200)), None);
    assert_eq!(album("Song", "Artist", "Single", Some(200)), None);
    assert_eq!(
        planned_queries("Teardrop", "Massive Attack", Some("Mezzanine"), None).len(),
        planned.len() - 1
    );
}

#[test]
fn no_album_query_when_the_other_tracks_filenames_would_pass_for_the_song() {
    // Every track on the record is "Talk Talk - NN - Name", so the title is in all of them.
    let album = |t: &str, a: &str, al: &str, d: i32| album_query(Some(t), Some(a), Some(al), Some(d));
    assert_eq!(album("Talk Talk", "Talk Talk", "The Party's Over", 200), None);
    assert_eq!(album("Wembley", "Queen", "Live at Wembley '86", 200), None);
    assert!(album("Bohemian Rhapsody", "Queen", "Live at Wembley '86", 340).is_some());
}

#[test]
fn a_short_title_must_appear_as_words_of_its_own() {
    assert!(!matches("anything at all.flac", "M.I.A."));
    assert!(!matches("Supper Club.flac", "Up"));
    assert!(matches("MIA.flac", "M.I.A."));
    assert!(matches("Peter Gabriel - 05 - Up.flac", "Up"));
}

// ---- quality ranking ------------------------------------------------------

fn hit(bit_depth: Option<i32>, sample_rate: Option<i32>) -> SoulseekFileHit {
    SoulseekFileHit {
        bit_depth,
        sample_rate,
        ..Default::default()
    }
}

/// A 24/96 transfer of a CD-era master carries no more music than the 16/44.1 one,
/// at several times the bytes and the transfer time. Ranked down, never rejected.
#[test]
fn cd_quality_outranks_hi_res() {
    let cd = quality_penalty(&hit(Some(16), Some(44100)));
    let cd48 = quality_penalty(&hit(Some(16), Some(48000)));
    let hi_res = quality_penalty(&hit(Some(24), Some(96000)));

    assert!(cd < cd48, "16/44.1 is the target, 16/48 is the runner-up");
    assert!(cd48 < hi_res, "hi-res sorts last");
}

/// Most peers report neither field. Treating unknown as hi-res would bury the
/// majority of a normal search; treating it as CD would let an unlabelled 24/96
/// outrank a labelled 16/44.1. It sits between the two on purpose.
#[test]
fn unknown_quality_sits_between_cd_and_hi_res() {
    let unknown = quality_penalty(&hit(None, None));

    assert!(quality_penalty(&hit(Some(16), Some(48000))) < unknown);
    assert!(unknown < quality_penalty(&hit(Some(24), Some(96000))));
}

/// Size is the last signal left and it ties often, because slskd reports queue length
/// and upload speed per response rather than per file. Pointing it the wrong way for
/// a lossy library walks every track down to the worst copy on the shelf.
#[test]
fn size_tiebreak_points_towards_cd_rips_but_away_from_low_bitrates() {
    assert!(
        size_sort_key(30_000_000, Some("flac")) > size_sort_key(25_000_000, Some("flac")),
        "chasing lossless, the smaller of two equals is the CD rip"
    );

    assert!(
        size_sort_key(10_000_000, Some("mp3")) < size_sort_key(4_000_000, Some("mp3")),
        "chasing lossy, the bigger file is simply the higher bitrate"
    );
}

/// A configured extension is normalized the same way a hit's is.
#[test]
fn size_tiebreak_reads_a_configured_extension_in_any_shape() {
    let smaller_first = size_sort_key(9, Some("flac"));
    assert_eq!(size_sort_key(9, Some(".flac")), smaller_first);
    assert_eq!(size_sort_key(9, Some("FLAC")), smaller_first);
}

#[test]
fn the_quality_penalties_follow_the_c_sharp_tables() {
    let cases = [
        ((Some(16), Some(44100)), 0),
        ((Some(8), Some(22050)), 9),
        ((Some(32), Some(192000)), 41),
        ((Some(24), Some(176400)), 30),
        ((Some(24), Some(352800)), 30),
        ((Some(24), Some(64000)), 20),
        ((Some(24), Some(88200)), 20),
    ];
    for ((depth, rate), expected) in cases {
        assert_eq!(quality_penalty(&hit(depth, rate)), expected, "{depth:?}/{rate:?}");
    }
}

// ---- SongIdentityTests: the Soulseek cases -----------------------------------------------

#[test]
fn soulseek_queries_skip_brackets_and_cap_the_searches() {
    let texts = |title: &str, artist: &str| -> Vec<String> {
        search_queries(title, artist)
            .iter()
            .map(SongQuery::text)
            .collect()
    };
    assert_eq!(
        texts("Long Season [LIVE][4K]", "Fishmans"),
        ["Fishmans Long Season", "Long Season"]
    );
    assert_eq!(
        texts("$UICIDE", "$uicideboy$"),
        ["$uicideboy$ $UICIDE", "suicideboys SUICIDE", "$UICIDE"]
    );
    assert_eq!(
        texts("(Exchange)", "Massive Attack"),
        ["Massive Attack Exchange", "Exchange"]
    );
    // Two with the artist at most, then the title alone.
    assert_eq!(
        search_queries("Ca$h (feat. Gue$t)", "A$AP Rocky feat. Gue$t").len(),
        3
    );
}

// ---- Soulseek: never a different version -------------------------------------------------

#[test]
fn soulseek_a_file_of_another_version_is_rejected() {
    let cases = [
        (r"music\Radiohead\OK Computer\03 - Creep (Live).flac", "Creep"),
        ("Glass Animals - Heat Waves (Sped Up).flac", "Heat Waves"),
        ("Heat Waves (Slowed + Reverb).flac", "Heat Waves"),
        ("08 - Group Four (Security Forces Dub).flac", "Group Four"),
        ("Song - Radio Edit.flac", "Song"),
        ("Song (Instrumental).flac", "Song"),
        ("Song (Live).flac", "Song (Remix)"),
    ];
    for (filename, title) in cases {
        assert!(adds_version(filename, title), "{filename} for {title}");
    }
}

#[test]
fn soulseek_the_same_version_is_kept() {
    let cases = [
        ("03 - Creep.flac", "Creep"),
        ("Creep (Remastered 2009).flac", "Creep"),
        ("Strobe (Original Mix).flac", "Strobe"),
        ("Song (Explicit).flac", "Song"),
        ("03 - Creep (Live).flac", "Creep (Live)"),
        ("Live Forever.flac", "Live Forever"),
        ("Hole - Live Through This - 03 - Doll Parts.flac", "Doll Parts"),
    ];
    for (filename, title) in cases {
        assert!(!adds_version(filename, title), "{filename} for {title}");
    }
}

#[test]
fn soulseek_a_file_named_another_way_still_matches_the_title() {
    let cases = [
        ("01 - Suicide.flac", "$UICIDE"),
        ("$uicideboy$ - $UICIDE.flac", "Suicide"),
        ("05 - Huntin' Wabbitz.flac", "Huntin\u{2019} Wabbitz"),
        ("Sigur Ros - Hoppipolla.flac", "Hopp\u{ed}polla"),
    ];
    for (filename, title) in cases {
        assert!(matches(filename, title), "{filename} for {title}");
        assert!(matches_phrase(filename, title), "{filename} for {title} (phrase)");
    }
}

#[test]
fn soulseek_ultimate_suicide_is_not_suicide_on_the_title_only_search() {
    assert!(!matches_phrase("Ultimate $uicide.flac", "$UICIDE Pt. 2"));
}

// ---- LiveVersionTests: the Soulseek cases ----------------------------------------------

const LIVE_PATH: &str =
    r"Music\Silverstein\Decade (live at the El Mocambo) (2010)\17 - Smile in Your Sleep.flac";

#[test]
fn a_file_in_a_live_albums_folder_is_not_the_studio_song() {
    assert!(from_live_folder(
        LIVE_PATH,
        "Smile in Your Sleep",
        Some("Sad Songs Vol. 1")
    ));
}

#[test]
fn the_studio_albums_folder_is_fine() {
    assert!(!from_live_folder(
        r"Music\Silverstein\Discovering the Waterfront (2005)\05 - Smile in Your Sleep.flac",
        "Smile in Your Sleep",
        None
    ));
}

#[test]
fn live_album_folders_are_read() {
    let paths = [
        r"Music\Queen\Live at Wembley '86\CD1\01 - One Vision.flac",
        r"Nirvana\MTV Unplugged in New York\01 - About a Girl.flac",
        r"Shares\Pearl Jam - 2000-06-25 Katowice [bootleg]\03 - Corduroy.flac",
        "Music/Artist/Album (Live)/01 - Song.flac",
    ];
    for path in paths {
        assert!(
            from_live_folder(path, "Song", Some("Some Studio Album")),
            "{path}"
        );
    }
}

#[test]
fn a_share_called_live_at_the_top_is_not_the_album() {
    // Only the album folder and the one above it say what the file is.
    assert!(!from_live_folder(
        r"Live Music\Rock\Silverstein\Discovering the Waterfront\05 - Smile in Your Sleep.flac",
        "Smile in Your Sleep",
        Some("Discovering the Waterfront")
    ));
}

#[test]
fn a_live_request_takes_a_live_folder() {
    let cases = [
        ("Smile in Your Sleep", Some("Decade (live at the El Mocambo)")),
        ("Smile in Your Sleep (Live)", None),
        ("Doll Parts", Some("Live Through This")),
    ];
    for (title, album) in cases {
        assert!(!from_live_folder(LIVE_PATH, title, album), "{title} / {album:?}");
    }
}

// ---- AlbumFolderTests: the pure statics --------------------------------------------------

#[test]
fn the_album_search_words() {
    let cases = [
        ("Drake", "HABIBTI (FOMO)", Some("Drake HABIBTI")),
        ("Artist", "Song - Single", Some("Artist Song")),
        ("Artist", "Deluxe [Remastered]", Some("Artist Deluxe")),
        ("Artist", "Unknown Album", None),
        ("", "Album", None),
    ];
    for (artist, album, expected) in cases {
        assert_eq!(
            album_search_text(Some(artist), Some(album)).as_deref(),
            expected,
            "{artist} / {album}"
        );
    }
}

#[test]
fn the_folder_of_a_file_is_what_the_peer_names_it() {
    let cases = [
        (r"Music\Artist\Album\01 - Song.mp3", r"Music\Artist\Album"),
        ("Music/Artist/01 - Song.mp3", "Music/Artist"),
        ("01 - Song.mp3", ""),
    ];
    for (file, folder) in cases {
        assert_eq!(folder_of_file(file), folder, "{file}");
    }
}
