//! `Services/Admin`: the dashboard's sign-ins and folder picker.

pub mod browse_session_store;
pub mod directory_browser;

pub use browse_session_store::BrowseSessionStore;
pub use directory_browser::{BrowseEntry, BrowseResult, DirectoryBrowser};
