//! `Services/Local`.

pub mod download_history_service;
pub mod i_local_library_service;
pub mod local_library_service;

pub use download_history_service::DownloadHistoryService;
pub use i_local_library_service::{ILocalLibraryService, ParsedExternalId, ParsedSongId};
pub use local_library_service::{LocalLibraryService, LocalSongMapping};

#[cfg(test)]
pub(crate) mod test_support;
