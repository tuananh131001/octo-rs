//! Port of `Services/IMusicMetadataService.cs`.
//!
//! STUB(4-A): replaced when 4-A lands; a copy of 4-A's trait, so 4-D's callers compile against
//! the same members.
//!
//! The C# methods that took a `CancellationToken` drop it here: a caller that gives up drops the
//! future, which stops the work at its next await as the token did.

use async_trait::async_trait;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;

/// Interface for external music metadata search service
/// (Deezer API, Spotify API, MusicBrainz, etc.)
#[async_trait]
pub trait IMusicMetadataService: Send + Sync {
    /// Searches for songs on external providers. `limit` was 20 by default.
    async fn search_songs(&self, query: &str, limit: i32) -> Vec<Song>;

    /// Searches for songs given an already-split artist and title. Preferred when
    /// the caller has structured fields (e.g. Last.fm results) so providers don't
    /// have to reverse-parse a concatenated query — which corrupts multi-word
    /// artists like "The Beatles" into artist="The", title="Beatles ...".
    /// `duration_seconds` if known is stored on the placeholder
    /// so the client's scrub bar shows the right total length on first play.
    /// (`limit` was 1 by default.)
    async fn search_songs_by_artist_title(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
        duration_seconds: Option<i32>,
    ) -> Vec<Song>;

    /// Fills in real duration/album/year on external placeholder songs (which
    /// otherwise show a 3:00 fallback) from a metadata provider. Bounded, cached,
    /// best-effort; does not block or fail the search if the provider is down.
    async fn enrich_external_songs(&self, _songs: &mut [Song]) {}

    /// Gives external songs outside a search (station playlists) the lengths already known
    /// for them, and looks the rest up in the background so the next response has them.
    /// Never waits on the network: whatever is not known yet keeps the fallback for now.
    fn complete_song_lengths(&self, _songs: &mut [Song]) {}

    /// Resolves the real YouTube video (and its duration) for the top of a search
    /// result so the shown length matches the audio that plays. Bounded + cached;
    /// also stores the videoId so playback reuses the same video. When `background`
    /// is true the songs are already sent: only the routing is written, a video a play already
    /// pinned is kept, and the shim's background lane is used.
    async fn resolve_top_durations(&self, _songs: &mut [Song], _background: bool) {}

    /// Best-effort pre-resolve of upstream identifiers (e.g. YouTube videoIds) for
    /// the first N songs of a freshly-built search result. Called fire-and-forget
    /// so search3 still returns instantly. Provider-specific (a Deezer/Qobuz
    /// implementation would no-op); default no-op preserves source compatibility
    /// for any provider that doesn't need it.
    async fn prewarm_you_tube_ids(&self, _songs: &[Song], _top_n: usize) {}

    /// Same as [`IMusicMetadataService::prewarm_you_tube_ids`] but accepts raw song ids — the
    /// implementation looks each id up in its own routing registry to find the
    /// artist/title to resolve. Used by the scrobble-driven sliding-window
    /// prewarm where the controller only has the scrobbled song id and the
    /// upcoming-songs list it stored at search time.
    async fn prewarm_you_tube_ids_for_song_ids(&self, _song_ids: &[String], _top_n: usize) {}

    /// Best-effort pre-fetch of cover art for the first N songs of a freshly-built
    /// search result, so a client that renders them a moment later finds the image
    /// already cached instead of triggering the fetch itself. Fire-and-forget, on
    /// each source's background rate-limit lane so it never queues behind a live
    /// search or cover request. Default no-op preserves source compatibility for
    /// any provider that doesn't need it.
    async fn prewarm_cover_art(&self, _songs: &[Song], _top_n: usize) {}

    /// Searches for albums on external providers. `limit` was 20 by default.
    async fn search_albums(&self, query: &str, limit: i32) -> Vec<Album>;

    /// Searches for artists on external providers. `limit` was 20 by default.
    async fn search_artists(&self, query: &str, limit: i32) -> Vec<Artist>;

    /// Combined search (songs, albums, artists). The limits were 20 each by default.
    async fn search_all(
        &self,
        query: &str,
        song_limit: i32,
        album_limit: i32,
        artist_limit: i32,
    ) -> SearchResult;

    /// Gets details of an external song
    async fn get_song(&self, external_provider: &str, external_id: &str) -> Option<Song>;

    /// Gets details of an external album with its songs
    async fn get_album(&self, external_provider: &str, external_id: &str) -> Option<Album>;

    /// Gets details of an external artist
    async fn get_artist(&self, external_provider: &str, external_id: &str) -> Option<Artist>;

    /// Gets an artist's albums
    async fn get_artist_albums(&self, external_provider: &str, external_id: &str) -> Vec<Album>;

    /// Gets an artist's albums for a library artist's page. The library's album titles say
    /// which of two artists of one name is meant; a provider that cannot use them ignores them.
    /// (The C# overload of `GetArtistAlbumsAsync` that takes `libraryAlbumTitles`.)
    async fn get_artist_albums_for_library(
        &self,
        external_provider: &str,
        external_id: &str,
        _library_album_titles: Option<&[String]>,
    ) -> Vec<Album> {
        self.get_artist_albums(external_provider, external_id).await
    }

    /// Gets an artist's albums with only the track counts already known, asking the catalog
    /// for none: for counts shown beside an artist's page, whose own album list asks for the
    /// missing ones at the same moment. A provider with no such lookup answers the plain list.
    async fn get_artist_albums_known_counts(&self, external_provider: &str, external_id: &str) -> Vec<Album> {
        self.get_artist_albums(external_provider, external_id).await
    }

    /// Searches for playlists on external providers. `limit` was 20 by default.
    async fn search_playlists(&self, query: &str, limit: i32) -> Vec<ExternalPlaylist>;

    /// Gets details of an external playlist (metadata only, not tracks).
    /// `external_provider` is the provider name (e.g., "deezer", "qobuz"), `external_id` the
    /// playlist ID from the provider. None if not found.
    async fn get_playlist(&self, external_provider: &str, external_id: &str) -> Option<ExternalPlaylist>;

    /// Gets all tracks from an external playlist.
    async fn get_playlist_tracks(&self, external_provider: &str, external_id: &str) -> Vec<Song>;
}
