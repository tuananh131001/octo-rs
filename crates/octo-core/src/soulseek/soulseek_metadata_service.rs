//! The routing types of `Services/Soulseek/SoulseekMetadataService.cs`.
//!
//! STUB(4-A soulseek): replaced when 4-A lands. Only the fields the cover-art sources read exist
//! here; the rest of `SoulseekRouting` (YouTubeId, Duration, Track, DiscNumber, TotalTracks,
//! Isrc, ShownDuration, ShownDurationSource, ExternalArtistId) comes with the port.

/// What a routing is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RoutingKind {
    #[default]
    Song = 0,
    Album = 1,
    Artist = 2,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SoulseekRouting {
    pub kind: RoutingKind,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,

    /// Deezer album id, when an album search resolved one. Absent on album
    /// routings minted from a song row, which fall back to a name lookup.
    pub external_album_id: Option<String>,
}
