//! `Services/Updates`: the release check. (`UpdateHost` is 2-B's.)

pub mod release_check;

pub use release_check::{ReleaseCheck, ReleaseCheckState, ReleaseCheckView, ReleaseNote};
