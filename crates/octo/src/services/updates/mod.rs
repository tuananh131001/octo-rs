//! Updates with I/O (`Services/Updates`): the GitHub release check and the handshake with the
//! host helper. Release versions are `octo_core::updates`.

pub mod release_check;
pub mod update_host;

pub use release_check::{ReleaseCheck, ReleaseCheckState, ReleaseCheckView, ReleaseNote};
pub use update_host::{UpdateHelperInfo, UpdateHost, UpdateRequestError, UpdateRunStates, UpdateRunStatus};
