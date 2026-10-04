//! `Services/Fingerprint/TrackMatchComparer.cs`, the one helper the MusicBrainz client needs.
//!
//! STUB(2-A tagging): replaced when 2-A lands; only `ArtistMatches` exists here, ported whole,
//! because `MusicBrainzClient.Pick` calls it.

use crate::common::{ArtistAgreement, SongIdentity};

pub struct TrackMatchComparer;

impl TrackMatchComparer {
    /// Whether the requested artist is among the credited ones. Absent on either side is not a
    /// contradiction.
    pub fn artist_matches(requested: &str, credited_joined: &str, credits: Option<&[String]>) -> bool {
        let a = SongIdentity::key(requested);
        let b = SongIdentity::key(credited_joined);
        if a.is_empty() || (b.is_empty() && credits.is_none_or(<[String]>::is_empty)) {
            return true;
        }

        let agreement = match credits {
            Some(listed) => SongIdentity::compare_artists_with_credits(requested, credited_joined, listed),
            None => SongIdentity::compare_artists(requested, credited_joined),
        };
        match agreement {
            ArtistAgreement::Agree | ArtistAgreement::Loose | ArtistAgreement::Unknown => true,
            ArtistAgreement::Conflict => false,
            ArtistAgreement::None => !b.is_empty() && (a.contains(&b) || b.contains(&a)),
        }
    }
}
