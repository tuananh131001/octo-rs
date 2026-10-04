//! `Services/Local` in the C#.

pub mod i_local_library_service;
pub mod local_library_service;

pub use i_local_library_service::ILocalLibraryService;
pub use local_library_service::{LocalLibraryService, LocalSongMapping};
