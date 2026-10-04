//! Updates with I/O (`Services/Updates`): the handshake with the host helper. Release versions
//! are `octo_core::updates`; the GitHub release check comes later.

pub mod update_host;

pub use update_host::{UpdateHelperInfo, UpdateHost, UpdateRequestError, UpdateRunStates, UpdateRunStatus};
