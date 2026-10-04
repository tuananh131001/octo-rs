//! Updates, the pure part (`Services/Updates`): release versions. The handshake with the host
//! helper and the GitHub release check are `octo::services::updates`.

pub mod release_version;

pub use release_version::ReleaseVersion;
