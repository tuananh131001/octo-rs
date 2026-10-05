//! Sets `OCTO_RELEASE_VERSION` for `octo_core::VERSION`: the release this build is, as the
//! dashboard, the User-Agent and the update check show it (`2026.10.04`).
//!
//! `OCTO_VERSION` in the build's environment wins (a CI or Docker build can stamp a release
//! without editing a file); otherwise the repository's `VERSION` file, which a release bumps
//! before it is tagged. `CARGO_PKG_VERSION` cannot carry the tag: semver drops the leading zero
//! of `03`, and the dashboard compares the tag name verbatim.

use std::path::Path;

fn main() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../VERSION");
    println!("cargo:rerun-if-env-changed=OCTO_VERSION");
    println!("cargo:rerun-if-changed={}", file.display());

    let version = match std::env::var("OCTO_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => match std::fs::read_to_string(&file) {
            Ok(text) => text.trim().to_string(),
            Err(e) => panic!(
                "{} cannot be read ({e}), and OCTO_VERSION is not set",
                file.display()
            ),
        },
    };
    if !is_release(&version) {
        panic!("the release version {version:?} is not a dated release like 2026.10.03 or 2026.10.03.2");
    }
    println!("cargo:rustc-env=OCTO_RELEASE_VERSION={version}");
}

/// `yyyy.MM.dd` or `yyyy.MM.dd.N`, optionally followed by `+build` metadata (which the
/// User-Agent and the update check strip): the shape of a release tag.
fn is_release(version: &str) -> bool {
    let tag = version.split('+').next().unwrap_or_default();
    let parts: Vec<&str> = tag.split('.').collect();
    let widths_ok = match parts.as_slice() {
        [y, m, d] => y.len() == 4 && m.len() == 2 && d.len() == 2,
        [y, m, d, n] => y.len() == 4 && m.len() == 2 && d.len() == 2 && (1..=4).contains(&n.len()),
        _ => false,
    };
    widths_ok && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
}
