//! Port of `Services/LastFm/LastFmRadioSpacing.cs`.
//!
//! Spreads a song radio's artists out. The similar-tracks list puts the seed's own album and
//! artist at the top, so played in order a radio opened with three songs from one album and
//! sounded like the album, not a radio.

use crate::common::SongIdentity;
use crate::common::dotnet;

/// How many other artists play before an artist comes round again.
pub const GAP: usize = 3;

/// The songs in their order, except that a song waits until [`GAP`] other artists have played
/// since its artist last did, counting the seed, which plays first. When every song left would
/// have to wait, the next in order plays anyway, so nothing is ever dropped.
pub fn spread<T, F>(songs: &[T], artist_of: F, seed_artist: Option<&str>) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> Option<String>,
{
    let mut pending: Vec<T> = songs.to_vec();
    let mut recent: Vec<String> = Vec::new();
    let mut spread = Vec::with_capacity(songs.len());
    played(&mut recent, seed_artist);
    while !pending.is_empty() {
        let next = pending
            .iter()
            .position(|song| !recent.contains(&artist_key(artist_of(song).as_deref())))
            .unwrap_or(0);
        let song = pending.remove(next);
        played(&mut recent, artist_of(&song).as_deref());
        spread.push(song);
    }
    spread
}

fn played(recent: &mut Vec<String>, artist: Option<&str>) {
    let key = artist_key(artist);
    if key.is_empty() {
        return;
    }
    recent.retain(|k| *k != key);
    recent.push(key);
    if recent.len() > GAP {
        recent.remove(0);
    }
}

/// The main artist, however the credit is written: "Drake feat. Future" is Drake.
fn artist_key(artist: Option<&str>) -> String {
    match artist {
        Some(artist) if !dotnet::is_blank(artist) => SongIdentity::key(&SongIdentity::primary_artist(artist)),
        _ => String::new(),
    }
}

/// LastFmRadioSpacingTests: a song radio spreads its artists out, so it does not open with
/// the seed's album.
#[cfg(test)]
mod tests {
    use super::*;

    fn spread_of(seed_artist: &str, songs: &[&str]) -> Vec<String> {
        let songs: Vec<String> = songs.iter().map(|s| s.to_string()).collect();
        spread(
            &songs,
            |s: &String| Some(s.split(" / ").next().unwrap_or_default().to_string()),
            Some(seed_artist),
        )
    }

    #[test]
    fn the_seeds_album_mates_wait_for_other_artists() {
        // The radio from "$uicideboy$ - BLOODSWEAT", 2026-10-02: the two songs at the top
        // were from the seed's own album.
        let spread = spread_of(
            "$uicideboy$",
            &[
                "$uicideboy$ / 2009 Reggie Bush",
                "$uicideboy$ / Angel Grove",
                "Scrim / Father, Hold Me",
                "Pouya / FIVE SIX",
                "Bones / HDMI",
                "Night Lovell / Alone",
                "$uicideboy$ / Matte Black",
            ],
        );
        assert_eq!(
            spread,
            [
                "Scrim / Father, Hold Me",
                "Pouya / FIVE SIX",
                "Bones / HDMI",
                "$uicideboy$ / 2009 Reggie Bush",
                "Night Lovell / Alone",
                "$uicideboy$ / Angel Grove",
                "$uicideboy$ / Matte Black",
            ]
        );
    }

    #[test]
    fn a_guest_credit_is_the_same_artist() {
        let spread = spread_of("Drake", &["Drake feat. Future / Life Is Good", "SZA / Kill Bill"]);
        assert_eq!(spread, ["SZA / Kill Bill", "Drake feat. Future / Life Is Good"]);
    }

    #[test]
    fn nothing_is_dropped_when_one_artist_fills_the_list() {
        let songs = ["Bones / A", "Bones / B", "Bones / C"];
        assert_eq!(spread_of("Bones", &songs), songs);
    }

    #[test]
    fn a_list_already_spread_keeps_its_order() {
        let songs = ["Scrim / A", "Pouya / B", "Bones / C", "Scrim / D", "Pouya / E"];
        assert_eq!(spread_of("$uicideboy$", &songs), songs);
    }

    #[test]
    fn songs_with_no_artist_are_never_held_back() {
        let songs = vec![String::new(), "x".to_string()];
        let spread = spread(
            &songs,
            |s: &String| if s.is_empty() { None } else { Some("A".to_string()) },
            None,
        );
        assert_eq!(spread, ["", "x"]);
    }
}
