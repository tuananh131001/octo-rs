//! The routing model declared at the bottom of `Services/Soulseek/SoulseekMetadataService.cs`:
//! [`RoutingKind`] and [`SoulseekRouting`], which `external-ids.json` carries.
//!
//! These two types are complete ports (checked against the `external-ids.json` fixture). The
//! service itself is task 4-A's and lives in `octo::services::soulseek::soulseek_metadata_service`.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::soulseek::song_length::LengthSource;

/// What a short external id stands for. Written as its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum RoutingKind {
    #[default]
    Song = 0,
    Album = 1,
    Artist = 2,
}

/// Where an outside song, album or artist came from, enough to find it again: what the short
/// external ids Octo hands to Subsonic clients stand for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SoulseekRouting {
    pub kind: RoutingKind,
    pub you_tube_id: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub duration: Option<i32>,

    /// Deezer album id, when an album search resolved one. Absent on album
    /// routings minted from a song row, which fall back to a name lookup.
    pub external_album_id: Option<String>,

    /// Deezer artist id behind an artist routing, once an artist search or an artist
    /// page settled on one. The id itself is minted from the name alone, and two artists can
    /// share a name, so this is what says which of them the page lists. Not part of the id.
    pub external_artist_id: Option<String>,

    /// Position within its album. Carried so a track downloaded as part of an
    /// album keeps its ordering: the download path rebuilds the song from its id alone,
    /// and without this every track lands untracked and sorts alphabetically.
    pub track: Option<i32>,

    /// Disc within a multi-disc release. Same reasoning as [`SoulseekRouting::track`].
    pub disc_number: Option<i32>,

    /// Track count of the album this came from. Without it the tagger fills the
    /// "x of y" denominator from a per-track Deezer search that can match a different
    /// release, producing nonsense like 5/10 on an 8-track album.
    pub total_tracks: Option<i32>,

    /// The track's ISRC, when the album listing that minted this routing named one.
    /// Carried for the same reason as [`SoulseekRouting::track`], and because it is the
    /// strongest evidence download verification can be given about which recording was asked
    /// for. Not part of the id, so routings minted before it existed keep their ids.
    pub isrc: Option<String>,

    /// The length shown for this song once a lookup found one, kept here so every
    /// later response carries it, across restarts too. Display only: see
    /// [`crate::soulseek::SongLength`] for why this is not [`SoulseekRouting::duration`].
    pub shown_duration: Option<i32>,

    /// Where [`SoulseekRouting::shown_duration`] came from, so a weaker source never
    /// replaces a stronger one.
    pub shown_duration_source: LengthSource,
}

impl SoulseekRouting {
    pub fn has_you_tube(&self) -> bool {
        self.you_tube_id.as_deref().is_some_and(|id| !id.is_empty())
    }

    pub fn has_artist_title(&self) -> bool {
        self.artist.as_deref().is_some_and(|a| !a.is_empty())
            && self.title.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// By hand, because System.Text.Json also wrote the computed `HasYouTube` and `HasArtistTitle`
/// (ignored again on read).
impl Serialize for SoulseekRouting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("SoulseekRouting", 16)?;
        s.serialize_field("Kind", &self.kind)?;
        s.serialize_field("YouTubeId", &self.you_tube_id)?;
        s.serialize_field("Artist", &self.artist)?;
        s.serialize_field("Title", &self.title)?;
        s.serialize_field("Album", &self.album)?;
        s.serialize_field("Duration", &self.duration)?;
        s.serialize_field("ExternalAlbumId", &self.external_album_id)?;
        s.serialize_field("ExternalArtistId", &self.external_artist_id)?;
        s.serialize_field("Track", &self.track)?;
        s.serialize_field("DiscNumber", &self.disc_number)?;
        s.serialize_field("TotalTracks", &self.total_tracks)?;
        s.serialize_field("Isrc", &self.isrc)?;
        s.serialize_field("ShownDuration", &self.shown_duration)?;
        s.serialize_field("ShownDurationSource", &self.shown_duration_source)?;
        s.serialize_field("HasYouTube", &self.has_you_tube())?;
        s.serialize_field("HasArtistTitle", &self.has_artist_title())?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_computed_flags_are_written_and_ignored_on_read() {
        let routing = SoulseekRouting {
            you_tube_id: Some("abc".into()),
            artist: Some("A".into()),
            title: Some(String::new()),
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(
            json.ends_with(r#""ShownDurationSource":0,"HasYouTube":true,"HasArtistTitle":false}"#),
            "{json}"
        );
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, routing);
    }
}
