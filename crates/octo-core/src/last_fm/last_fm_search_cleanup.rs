//! Port of `Services/LastFm/LastFmSearchCleanup.cs`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::last_fm_service::SimilarTrack;
use crate::common::SongIdentity;
use crate::common::dotnet;

const PLACEHOLDERS: [&str; 4] = ["unknown", "unknownartist", "notavailable", "na"];

// A hyphen, en dash or em dash (U+2013, U+2014), with or without space around it: "A - T", "A-T".
static DASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[-\u{2013}\u{2014}]\s*").expect("a valid pattern"));

/// Puts right the rows Last.fm's track.search gets from mislabelled scrobbles, before they become
/// outside songs. A search for "Kavinsky Nightcall" answers, among the real song, artist
/// "Kavinsky Nightcall" with title "Nightcall", title "Kavinsky - Nightcall" under the label
/// "Record Makers", and the same under "[unknown]". Taken as they are, those rows reach search
/// results, playback and each listener's Last.fm under the wrong names.
///
/// Only the song's own names are moved about, and only on evidence from the same answer. The
/// row's own artist at the front of its title is always taken out. Moving a song to another
/// artist needs two things: that artist has rows of their own, and the song as repaired is
/// among them, so Radiohead's "Creep - Acoustic" stays Radiohead's even when a band called
/// Creep answers the same search. What cannot be put right is left as it was, except a row
/// with no artist at all. Rows that turn out to be one song then keep the place of the first
/// and the spelling of the most listened.
pub struct LastFmSearchCleanup;

impl LastFmSearchCleanup {
    pub fn clean(tracks: &[SimilarTrack]) -> Vec<SimilarTrack> {
        // Every artist the answer names on its own, keyed loosely, with the spelling of its most
        // listened row.
        let mut known: HashMap<String, (String, i64)> = HashMap::new();
        for track in tracks {
            let key = SongIdentity::key(&track.artist);
            if key.is_empty() || is_placeholder(&track.artist) {
                continue;
            }
            let listeners = track.listeners.unwrap_or(0);
            let replace = match known.get(&key) {
                None => true,
                Some((_, current)) => listeners > *current,
            };
            if replace {
                known.insert(key, (track.artist.trim().to_string(), listeners));
            }
        }

        let songs: HashSet<String> = tracks
            .iter()
            .filter(|track| !is_placeholder(&track.artist))
            .map(|track| SongIdentity::match_key(&track.artist, &track.title))
            .collect();

        let cleaned: Vec<SimilarTrack> = tracks
            .iter()
            .filter_map(|track| repair(track, &known, &songs))
            .collect();

        // One song, several rows: the first place, the most listened spelling.
        let mut order: Vec<String> = Vec::new();
        let mut best: HashMap<String, SimilarTrack> = HashMap::new();
        for track in cleaned {
            let key = SongIdentity::match_key(&track.artist, &track.title);
            match best.get(&key) {
                None => {
                    order.push(key.clone());
                    best.insert(key, track);
                }
                Some(kept) if track.listeners.unwrap_or(0) > kept.listeners.unwrap_or(0) => {
                    best.insert(key, track);
                }
                Some(_) => {}
            }
        }
        order.into_iter().filter_map(|key| best.remove(&key)).collect()
    }
}

fn repair(
    track: &SimilarTrack,
    known: &HashMap<String, (String, i64)>,
    songs: &HashSet<String>,
) -> Option<SimilarTrack> {
    let artist = track.artist.trim();
    let title = track.title.trim();
    let artist_key = SongIdentity::key(artist);

    // "Artist - Title" in the title: under the artist itself ("Kanye West - Stronger" by Kanye
    // West), under no artist, or under someone else the answer names on their own, as a label
    // uploading the song ("Kavinsky - Nightcall" by Record Makers).
    if let Some((named, rest)) = split_at_artist(title, |candidate, rest| {
        SongIdentity::key(candidate) == artist_key
            || (known.contains_key(&SongIdentity::key(candidate))
                && songs.contains(&SongIdentity::match_key(candidate, rest)))
    }) {
        let named_key = SongIdentity::key(&named);
        let mut repaired = track.clone();
        repaired.artist = match known.get(&named_key) {
            Some((spelled, _)) => spelled.clone(),
            None => named,
        };
        repaired.title = rest;
        return Some(repaired);
    }

    if is_placeholder(artist) {
        // With no artist to go on, "A - T" still names one, at the first spaced dash.
        return match title.find(" - ") {
            Some(spaced) if spaced > 0 && spaced + 3 < title.len() => {
                let mut repaired = track.clone();
                repaired.artist = title[..spaced].trim().to_string();
                repaired.title = title[spaced + 3..].trim().to_string();
                Some(repaired)
            }
            _ => None,
        };
    }

    // The title on the end of the artist: "Kavinsky Nightcall" or "Kavinsky - Nightcall" with
    // title "Nightcall", when "Kavinsky" is an artist in its own right in the same answer.
    let title_key = SongIdentity::key(title);
    if !title_key.is_empty()
        && dotnet::utf16_len(&artist_key) > dotnet::utf16_len(&title_key)
        && artist_key.ends_with(&title_key)
        && let Some(cut) = ends_with_ignore_case(artist, title)
    {
        let prefix = DASH.replace_all(&artist[..cut], " ").trim().to_string();
        if let Some((spelled, _)) = known.get(&SongIdentity::key(&prefix))
            && songs.contains(&SongIdentity::match_key(&prefix, title))
        {
            let mut repaired = track.clone();
            repaired.artist = spelled.clone();
            return Some(repaired);
        }
    }

    Some(track.clone())
}

/// The title split at the dash after an artist `accept` takes, with the rest as the title,
/// trying each dash in turn so a name with a hyphen in it ("Jay-Z - 99 Problems") is read whole.
fn split_at_artist(title: &str, accept: impl Fn(&str, &str) -> bool) -> Option<(String, String)> {
    for dash in DASH.find_iter(title) {
        let before = title[..dash.start()].trim();
        let after = title[dash.end()..].trim();
        if before.is_empty() || after.is_empty() {
            continue;
        }
        if accept(before, after) {
            return Some((before.to_string(), after.to_string()));
        }
    }
    None
}

fn is_placeholder(artist: &str) -> bool {
    let key = SongIdentity::key(artist);
    PLACEHOLDERS.contains(&key.as_str()) || key.is_empty()
}

/// `text.EndsWith(suffix, StringComparison.OrdinalIgnoreCase)`, answering where the matched end
/// starts in `text` (`text[..^suffix.Length]` is then `text[..cut]`).
fn ends_with_ignore_case(text: &str, suffix: &str) -> Option<usize> {
    let count = suffix.chars().count();
    let total = text.chars().count();
    if count > total {
        return None;
    }
    let cut = text
        .char_indices()
        .nth(total - count)
        .map_or(text.len(), |(index, _)| index);
    dotnet::eq_ignore_case(&text[cut..], suffix).then_some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(artist: &str, title: &str, listeners: i64) -> SimilarTrack {
        SimilarTrack::new(artist, title, 1.0).with_listeners(Some(listeners))
    }

    fn clean(tracks: &[SimilarTrack]) -> Vec<(String, String)> {
        LastFmSearchCleanup::clean(tracks)
            .into_iter()
            .map(|track| (track.artist, track.title))
            .collect()
    }

    fn pair(artist: &str, title: &str) -> (String, String) {
        (artist.to_string(), title.to_string())
    }

    #[test]
    fn kavinsky_nightcall_as_last_fm_answers_it_is_the_real_song_once() {
        // Last.fm's answer for "Kavinsky Nightcall" on 2026-09-30, in its order.
        let rows = clean(&[
            t("Kavinsky", "Kavinsky - Nightcall", 1324),
            t("Kavinsky Nightcall", "Nightcall", 44),
            t("Kavinsky", "Nightcall", 1257699),
            t("Record Makers", "Kavinsky - Nightcall", 252),
            t("[unknown]", "Kavinsky - Nightcall", 131),
            t("<Unknown>", "Kavinsky - Nightcall", 129),
            t("Kavinsky & Lovefoxxx", "Nightcall", 114928),
            t("Kavinsky \u{2013} Nightcall", "Nightcall", 73),
        ]);

        assert_eq!(rows[0], pair("Kavinsky", "Nightcall"));
        assert_eq!(
            rows.iter()
                .filter(|r| **r == pair("Kavinsky", "Nightcall"))
                .count(),
            1
        );
        assert!(
            !rows
                .iter()
                .any(|(artist, title)| artist.contains("Nightcall") || title.starts_with("Kavinsky")),
            "{rows:?}"
        );
        assert!(
            !rows
                .iter()
                .any(|(artist, _)| matches!(artist.as_str(), "[unknown]" | "<Unknown>" | "Record Makers")),
            "{rows:?}"
        );
    }

    #[test]
    fn kanye_west_stronger_the_artist_in_the_title_is_taken_out_and_the_most_listened_spelling_kept() {
        let rows = clean(&[
            t("Kanye West", "Kanye West - Stronger", 2342),
            t("Kanye West", "Kanye west -stronger", 1143),
            t("Kanye West", "Kanye West-Stronger", 370),
            t("Kanye", "Kanye West - Stronger", 56),
            t("Kanye West", "Stronger", 2977765),
        ]);

        assert_eq!(rows, vec![pair("Kanye West", "Stronger")]);
    }

    #[test]
    fn a_hyphen_in_the_artist_is_read_whole() {
        assert_eq!(
            clean(&[t("Jay-Z", "Jay-Z - 99 Problems", 10)]),
            vec![pair("Jay-Z", "99 Problems")]
        );
    }

    #[test]
    fn titles_and_artists_that_only_look_alike_are_left_as_they_are() {
        let rows = clean(&[
            t("Earth, Wind & Fire", "Fire", 500000),
            t("Radiohead", "Creep - Acoustic", 90000),
            t("The Weeknd", "Blinding Lights", 3000000),
        ]);

        assert_eq!(
            rows,
            vec![
                pair("Earth, Wind & Fire", "Fire"),
                pair("Radiohead", "Creep - Acoustic"),
                pair("The Weeknd", "Blinding Lights"),
            ]
        );
    }

    #[test]
    fn another_artist_at_the_front_of_a_title_needs_their_own_row_of_that_song() {
        // A search for "creep" answers with a band called Creep too; Radiohead keeps its song.
        let rows = clean(&[
            t("Radiohead", "Creep", 4234650),
            t("Creep", "You", 9000),
            t("Radiohead", "Creep - Acoustic", 90000),
        ]);

        assert!(rows.contains(&pair("Radiohead", "Creep - Acoustic")), "{rows:?}");
        assert!(!rows.contains(&pair("Creep", "Acoustic")), "{rows:?}");
    }

    #[test]
    fn an_artist_with_the_title_on_the_end_is_only_trimmed_when_the_answer_names_the_shorter_artist() {
        assert_eq!(
            clean(&[t("Kavinsky Nightcall", "Nightcall", 44)]),
            vec![pair("Kavinsky Nightcall", "Nightcall")]
        );
    }

    #[test]
    fn a_row_with_no_artist_takes_it_from_the_title_or_is_dropped() {
        assert_eq!(
            clean(&[t("[unknown]", "Daft Punk - One More Time", 20)]),
            vec![pair("Daft Punk", "One More Time")]
        );
        assert!(clean(&[t("<Unknown>", "Track 01", 20)]).is_empty());
    }

    /// Rust-only: `EndsWith(OrdinalIgnoreCase)` and the cut it gives.
    #[test]
    fn ends_with_ignore_case_answers_where_the_suffix_starts() {
        assert_eq!(ends_with_ignore_case("Kavinsky Nightcall", "NIGHTCALL"), Some(9));
        assert_eq!(ends_with_ignore_case("Sigur Rós", "rÓs"), Some(6));
        assert_eq!(ends_with_ignore_case("a", "ab"), None);
        assert_eq!(ends_with_ignore_case("abc", "x"), None);
    }
}
