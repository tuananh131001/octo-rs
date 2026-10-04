//! The domain, search, download and Subsonic models: `Models/**` in the C#.
//!
//! Field names serialise as the C# PascalCase property names, since System.Text.Json wrote them
//! with no naming policy wherever these reach a file.

use serde::{Deserialize, Deserializer};

/// For a C# property that is a non-nullable reference type (a `string` or a `List<T>`):
/// System.Text.Json read a JSON `null` into it without complaint, where serde would fail the
/// whole file. Read `null` as the default instead.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

pub mod domain {
    pub mod album;
    pub mod artist;
    pub mod song;

    pub use album::Album;
    pub use artist::Artist;
    pub use song::Song;
}

pub mod search {
    pub mod search_result;

    pub use search_result::SearchResult;
}

pub mod download {
    pub mod download_history_entry;
    pub mod download_info;
    pub mod download_status;

    pub use download_history_entry::DownloadHistoryEntry;
    pub use download_info::DownloadInfo;
    pub use download_status::DownloadStatus;
}

pub mod radio {
    // STUB(5-C): the station types only, until 5-C ports the whole file.
    pub mod last_fm_radio_state;

    pub use last_fm_radio_state::{LastFmRadioStation, LastFmRadioTrack};
}

pub mod subsonic {
    pub mod external_playlist;
    pub mod scan_status;

    pub use external_playlist::ExternalPlaylist;
    pub use scan_status::ScanStatus;
}
