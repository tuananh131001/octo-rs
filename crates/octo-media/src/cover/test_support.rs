//! What the cover tests share.

use std::path::{Path, PathBuf};

use parking_lot::RwLock;

/// Held for reading by every test that paints covers, and for writing by the timing test, so
/// the timing test measures a cover and not the rest of the suite (C#'s non-parallel collection).
pub(crate) static HEAVY: RwLock<()> = RwLock::new(());

/// A path in the repository.
pub(crate) fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(path)
}
