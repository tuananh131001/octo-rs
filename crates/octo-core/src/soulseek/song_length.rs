//! Port of `Services/Soulseek/SongLength.cs`: the rules for the length a client is shown for a
//! song Octo found outside the library.

use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::soulseek::soulseek_metadata_service::SoulseekRouting;

/// Where the length shown for an outside song came from, weakest first. A shown length is
/// only ever replaced by one from the same or a stronger source.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize_repr, Deserialize_repr,
)]
#[repr(i32)]
pub enum LengthSource {
    /// Nothing is known. The song goes out with the 180s placeholder, which the
    /// Octo app reads as "no length".
    #[default]
    None = 0,

    /// The length of a YouTube video found for the song. Weakest because a video
    /// can carry an intro, an outro or a whole music-video skit.
    Video = 1,

    /// Last.fm's length: the one handed in with a radio track, or track.getInfo.
    /// Often missing, rarely wrong.
    LastFm = 2,

    /// Deezer's catalog length for the matched track.
    Deezer = 3,
}

/// The rules for the length a client is shown for a song Octo found outside the library.
///
/// This is display only. It lives on [`SoulseekRouting::shown_duration`], never on
/// [`SoulseekRouting::duration`], because Duration is what a download ranks peer files by and
/// checks the finished file against, to within 8 seconds. A length filled in so a row can
/// show one must not start rejecting files a download used to accept.
///
/// The C# locked the routing object around each read and write; in Rust a shared routing sits
/// behind a mutex (`octo::services::soulseek::external_id_registry::SharedRouting`), so these
/// take the routing the caller already holds locked.
pub struct SongLength;

impl SongLength {
    /// Shorter than this, a video is a clip or a teaser, not the song.
    pub const MIN_VIDEO_SECONDS: i32 = 30;

    /// Longer than this, a video is a live set, a mix or a whole album.
    pub const MAX_VIDEO_SECONDS: i32 = 20 * 60;

    /// The video's length when it is plausibly one song, or None.
    pub fn sane_video_length(seconds: Option<i32>) -> Option<i32> {
        seconds.filter(|&s| (Self::MIN_VIDEO_SECONDS..=Self::MAX_VIDEO_SECONDS).contains(&s))
    }

    /// Store a length on the routing unless a stronger source already gave one. Returns
    /// whether it was stored. A missing or zero length is never stored: no length beats a
    /// made-up one.
    pub fn remember(routing: &mut SoulseekRouting, seconds: Option<i32>, source: LengthSource) -> bool {
        let Some(s) = seconds else { return false };
        if s <= 0 || source == LengthSource::None {
            return false;
        }
        if source == LengthSource::Video && Self::sane_video_length(Some(s)).is_none() {
            return false;
        }
        if routing.shown_duration_source > source {
            return false;
        }
        routing.shown_duration = Some(s);
        routing.shown_duration_source = source;
        true
    }

    /// The shown length and where it came from, read together.
    pub fn shown(routing: &SoulseekRouting) -> (Option<i32>, LengthSource) {
        (routing.shown_duration, routing.shown_duration_source)
    }

    /// A length from a metadata source is known. A video length is only a
    /// stand-in, and still worth asking Deezer and Last.fm about.
    pub fn has_metadata_length(routing: &SoulseekRouting) -> bool {
        Self::shown(routing).1 >= LengthSource::LastFm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routing(artist: &str, title: &str) -> SoulseekRouting {
        SoulseekRouting {
            artist: Some(artist.into()),
            title: Some(title.into()),
            ..Default::default()
        }
    }

    #[test]
    fn sane_video_length_keeps_only_what_could_be_one_song() {
        let cases = [
            (29, None),
            (30, Some(30)),
            (417, Some(417)),
            (1200, Some(1200)),
            (1201, None),
            (3600, None),
        ];
        for (seconds, expected) in cases {
            assert_eq!(
                SongLength::sane_video_length(Some(seconds)),
                expected,
                "{seconds}"
            );
        }
    }

    #[test]
    fn remember_stronger_source_replaces_weaker_never_the_other_way() {
        let mut r = routing("Daft Punk", "Emotion");

        assert!(SongLength::remember(&mut r, Some(430), LengthSource::Video));
        assert!(SongLength::remember(&mut r, Some(418), LengthSource::LastFm));
        assert!(SongLength::remember(&mut r, Some(417), LengthSource::Deezer));
        assert_eq!(SongLength::shown(&r), (Some(417), LengthSource::Deezer));

        assert!(!SongLength::remember(&mut r, Some(418), LengthSource::LastFm));
        assert!(!SongLength::remember(&mut r, Some(430), LengthSource::Video));
        assert_eq!(SongLength::shown(&r), (Some(417), LengthSource::Deezer));
    }

    #[test]
    fn remember_never_stores_a_missing_zero_or_implausible_length() {
        let mut r = routing("A", "T");

        assert!(!SongLength::remember(&mut r, None, LengthSource::Deezer));
        assert!(!SongLength::remember(&mut r, Some(0), LengthSource::LastFm));
        assert!(!SongLength::remember(&mut r, Some(3600), LengthSource::Video));
        assert!(!SongLength::remember(&mut r, Some(12), LengthSource::Video));

        assert_eq!(SongLength::shown(&r), (None, LengthSource::None));
    }

    #[test]
    fn remember_a_long_metadata_length_is_not_bound_like_a_video() {
        // The range guards against a video carrying more than the song. A catalog length
        // for a 25-minute track is simply the track.
        let mut r = routing("A", "T");
        assert!(SongLength::remember(&mut r, Some(1500), LengthSource::Deezer));
    }

    #[test]
    fn has_metadata_length_is_last_fm_or_better() {
        let mut r = routing("A", "T");
        assert!(!SongLength::has_metadata_length(&r));
        SongLength::remember(&mut r, Some(200), LengthSource::Video);
        assert!(!SongLength::has_metadata_length(&r));
        SongLength::remember(&mut r, Some(201), LengthSource::LastFm);
        assert!(SongLength::has_metadata_length(&r));
    }
}
