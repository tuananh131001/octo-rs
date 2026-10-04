//! `Services/Common`: the shared services with state or I/O.

// STUB(4-D): the snapshot type only.
pub mod acquisition_tracker;
pub mod download_concurrency;
pub mod soulseek_hold_store;

pub use download_concurrency::{DownloadConcurrency, TransferLimiter, TransferSlot};
pub use soulseek_hold_store::{HeldAcquisition, HeldKind, SoulseekHoldStore};
