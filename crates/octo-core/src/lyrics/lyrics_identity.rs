//! Port of `Services/Lyrics/LyricsIdentity.cs`.

use std::sync::LazyLock;

use super::lyrics_models::LyricsQuery;
use crate::common::dotnet::{format_optional_decimals, is_null_or_white_space};
use crate::common::{SongIdentity, SongMatchOptions, SongQuery};

/// Lengths are compared by [`LyricsIdentity::length_fits`], where a source knows one. A clean
/// edit counts as the song here: its lyrics are the same words at the same times, with a few
/// bleeped, so its timing fits the explicit recording. Downloads stay strict.
static TITLES: LazyLock<SongMatchOptions> = LazyLock::new(|| SongMatchOptions {
    length_tolerance_seconds: None,
    also_neutral: vec!["clean".to_string()],
    ..SongMatchOptions::default()
});

/// Whether a lyrics entry is the song asked for. Strict on purpose: a lyrics search is loose and
/// returns the artist's other songs, and a length alone once let one of those stand in
/// ("Ultimate $uicide" for "$UICIDE", both about 170 s). So, by [`SongIdentity`]:
///
/// 1. The same title, whole: no prefix or containment rule, which is how "Ultimate $uicide"
///    would pass. Case, accents, punctuation, stylized characters, guests and upload noise such
///    as "(Explicit)" or "(Official Video)" are ignored.
/// 2. The same kind of recording: a remix, a live take, a sped-up upload never stands in for
///    the original, nor the original for them. A remaster is the same recording.
/// 3. The same artist: a primary artist of one side is credited on the other, and the two do
///    not name different guests. KuGou's "、" and its "Ye (侃爷)" read as they should.
///
/// The length is checked separately ([`LyricsIdentity::length_fits`]), because not every source
/// knows it.
pub struct LyricsIdentity;

impl LyricsIdentity {
    /// How far apart two lengths may be and still be one recording.
    pub const LENGTH_TOLERANCE_SECONDS: i32 = SongIdentity::LENGTH_TOLERANCE_SECONDS;

    /// `got_credits` lists the entry's artists one by one, when its source does.
    pub fn same_song<S: AsRef<str>>(
        want_title: &str,
        want_artist: &str,
        got_title: Option<&str>,
        got_artist: Option<&str>,
        got_credits: Option<&[S]>,
    ) -> bool {
        let Some(got_title) = got_title.filter(|title| !is_null_or_white_space(Some(title))) else {
            return false;
        };
        if is_null_or_white_space(Some(want_artist)) {
            return false;
        }
        let credit = Self::credit(got_artist, got_credits);
        SongIdentity::same_text(
            want_title,
            want_artist,
            got_title,
            credit.as_deref().unwrap_or(""),
            Some(&TITLES),
        )
        .is_same()
    }

    pub fn same_title(want: &str, got: Option<&str>) -> bool {
        got.is_some_and(|got| {
            !is_null_or_white_space(Some(got)) && SongIdentity::same_title(want, got, Some(&TITLES)).is_same()
        })
    }

    pub fn same_artist<S: AsRef<str>>(want: &str, got: Option<&str>, credits: Option<&[S]>) -> bool {
        SongIdentity::artists_agree_with_credits(want, got.unwrap_or(""), credits.unwrap_or(&[]))
    }

    /// A source that lists its artists one by one, as one credit.
    fn credit<S: AsRef<str>>(artist: Option<&str>, credits: Option<&[S]>) -> Option<String> {
        let listed: Vec<&str> = credits
            .unwrap_or(&[])
            .iter()
            .map(AsRef::as_ref)
            .filter(|credit| !is_null_or_white_space(Some(credit)))
            .collect();
        if is_null_or_white_space(artist) && !listed.is_empty() {
            Some(listed.join("、"))
        } else {
            artist.map(str::to_string)
        }
    }

    /// The searches a lyrics source tries for a song, the first as asked, then cleaned, with
    /// stylized characters read as letters, and by the primary artist alone, from
    /// [`SongIdentity::query_variants`]. Each has an artist: a lyrics search by title
    /// alone returns every song of that name, and the sources that need one have their own way
    /// to look past a renamed artist. At most `max` (three in C#), so a miss costs a source
    /// three requests. Whatever they find is still held to [`LyricsIdentity::same_song`]
    /// against the song as asked.
    pub fn searches(query: &LyricsQuery, max: usize) -> Vec<SongQuery> {
        let searches: Vec<SongQuery> = SongIdentity::query_variants(&query.title, &query.artist)
            .into_iter()
            .filter(|search| !search.artist.is_empty())
            .take(max)
            .collect();
        if searches.is_empty() {
            vec![SongQuery::new(query.title.clone(), query.artist.clone())]
        } else {
            searches
        }
    }

    /// Within a few seconds, or unknown on either side.
    pub fn length_fits(want: Option<i32>, got: Option<f64>) -> bool {
        SongIdentity::length_fits(want, got, SongIdentity::LENGTH_TOLERANCE_SECONDS)
    }

    /// Why a match that passed is still worth a look, or None.
    pub fn doubt(want: Option<i32>, got: Option<f64>) -> Option<String> {
        let Some(want) = want.filter(|want| *want > 0) else {
            return Some("the song's length is unknown".to_string());
        };
        let Some(got) = got.filter(|got| *got > 0.0) else {
            return Some("the lyrics carry no length".to_string());
        };
        let apart = (got - f64::from(want)).abs();
        (apart > 1.5).then(|| format!("lengths differ by {} s", format_optional_decimals(apart, 1)))
    }

    /// The title as compared: see [`SongIdentity::parse_title`].
    pub fn title_key(title: &str) -> String {
        SongIdentity::parse_title(title, None).key
    }

    /// The first-named artist, so "Drake feat. Rihanna" and "Drake" agree, as a key.
    pub fn lead_artist(artist: &str) -> String {
        SongIdentity::key(&SongIdentity::primary_artist(artist))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LyricsWordTimingTests.Identity_SameTitleSameKindSameArtist.
    #[test]
    fn identity_same_title_same_kind_same_artist() {
        let cases = [
            ("$UICIDE", "$uicideboy$", "$UICIDE", "$uicideboy$", true),
            ("$UICIDE", "$uicideboy$", "Ultimate $uicide", "$uicideboy$", false),
            ("$UICIDE", "$uicideboy$", "Black $uicide", "$uicideboy$", false),
            ("Headlines", "Drake", "Headlines (Explicit)", "Drake", true),
            (
                "Nightcall",
                "Kavinsky",
                "Nightcall (Breakbot Remix)",
                "Kavinsky",
                false,
            ),
            ("Nightcall (Live)", "Kavinsky", "Nightcall", "Kavinsky", false),
            (
                "Teardrop",
                "Massive Attack",
                "Teardrop - Remastered 2006",
                "Massive Attack",
                true,
            ),
            ("Shotta Flow", "NLE Choppa", "Shotta Flow 4", "NLE Choppa", false),
            (
                "Peek A Boo",
                "Lil Yachty",
                "Peek a Boo",
                "Lil Yachty、Migos",
                true,
            ),
            (
                "Work",
                "Rihanna",
                "Work (feat. Drake)",
                "Rihanna feat. Drake",
                true,
            ),
            // KuGou names Kanye West "Ye (侃爷)": the alias table knows him, the bracket is ignored.
            ("Stronger", "Kanye West", "Stronger", "Ye (侃爷)", true),
            ("Unsteady", "X Ambassadors", "Unsteady", "X Ambassadors", true),
            ("Stronger", "Kanye West", "Stronger", "Kelly Clarkson", false),
        ];
        for (title, artist, got_title, got_artist, same) in cases {
            // gotArtist.Split('、', StringSplitOptions.TrimEntries)
            let credits: Vec<&str> = got_artist.split('、').map(str::trim).collect();
            assert_eq!(
                LyricsIdentity::same_song(title, artist, Some(got_title), Some(got_artist), Some(&credits)),
                same,
                "{title} / {artist} against {got_title} / {got_artist}"
            );
        }
    }

    /// LyricsWordTimingTests.Identity_AnotherCreditedArtistCounts.
    #[test]
    fn identity_another_credited_artist_counts() {
        assert!(LyricsIdentity::same_artist(
            "Daft Punk",
            Some("Ye (侃爷)、Daft Punk"),
            Some(&["Ye (侃爷)", "Daft Punk"])
        ));
    }

    /// LyricsWordTimingTests.Identity_LengthWithinThreeSeconds.
    #[test]
    fn identity_length_within_three_seconds() {
        for (want, got, fits) in [
            (Some(169), 170.0, true),
            (Some(169), 173.0, false),
            (None, 400.0, true),
        ] {
            assert_eq!(
                LyricsIdentity::length_fits(want, Some(got)),
                fits,
                "{want:?} / {got}"
            );
        }
    }

    #[test]
    fn a_blank_title_or_artist_is_never_the_song() {
        let none: Option<&[&str]> = None;
        assert!(!LyricsIdentity::same_song(
            "Song",
            "Artist",
            Some(" "),
            Some("Artist"),
            none
        ));
        assert!(!LyricsIdentity::same_song(
            "Song",
            " ",
            Some("Song"),
            Some("Artist"),
            none
        ));
        assert!(!LyricsIdentity::same_song(
            "Song",
            "Artist",
            None,
            Some("Artist"),
            none
        ));
        // No artist on the entry, but its credits name one.
        assert!(LyricsIdentity::same_song(
            "Song",
            "Artist",
            Some("Song"),
            None,
            Some(&["Artist"])
        ));
    }

    #[test]
    fn doubt_says_what_could_not_be_checked() {
        assert_eq!(
            LyricsIdentity::doubt(None, Some(200.0)).as_deref(),
            Some("the song's length is unknown")
        );
        assert_eq!(
            LyricsIdentity::doubt(Some(200), Some(0.0)).as_deref(),
            Some("the lyrics carry no length")
        );
        assert_eq!(LyricsIdentity::doubt(Some(200), Some(201.5)), None);
        assert_eq!(
            LyricsIdentity::doubt(Some(200), Some(202.25)).as_deref(),
            Some("lengths differ by 2.3 s")
        );
        assert_eq!(
            LyricsIdentity::doubt(Some(200), Some(197.0)).as_deref(),
            Some("lengths differ by 3 s")
        );
    }

    #[test]
    fn searches_always_carry_an_artist_and_stop_at_the_limit() {
        let query = LyricsQuery::new("$uicideboy$", "$UICIDE (feat. Someone)", None, None);
        let searches = LyricsIdentity::searches(&query, 3);
        assert!(!searches.is_empty() && searches.len() <= 3);
        assert!(searches.iter().all(|search| !search.artist.is_empty()));
        assert_eq!(
            searches[0],
            SongQuery::new("$UICIDE (feat. Someone)", "$uicideboy$")
        );

        let no_artist = LyricsQuery::new("", "Song", None, None);
        assert_eq!(
            LyricsIdentity::searches(&no_artist, 3),
            vec![SongQuery::new("Song", "")]
        );
    }

    #[test]
    fn keys_ignore_guests_and_versions_the_way_song_identity_does() {
        assert_eq!(
            LyricsIdentity::lead_artist("Drake feat. Rihanna"),
            LyricsIdentity::lead_artist("Drake")
        );
        assert_eq!(
            LyricsIdentity::title_key("Teardrop (Explicit)"),
            LyricsIdentity::title_key("teardrop")
        );
    }
}
