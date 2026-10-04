//! Updates: the pure parts of `Services/Updates`.
//!
//! STUB(2-B): only `release_version` exists yet, as the release check (3-F) needs it; replaced
//! when 2-B lands.

pub mod release_version;

pub use release_version::ReleaseVersion;
