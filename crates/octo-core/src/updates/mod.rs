//! Updates, the pure part (`Services/Updates`): release versions. The handshake with the host
//! helper is `octo::services::updates::update_host`; the GitHub release check comes later.

pub mod release_version;

pub use release_version::ReleaseVersion;
