//! `Services/Updates/ReleaseVersion.cs`.
//!
//! STUB(2-B): replaced when 2-B lands. A faithful port of the C# type so the release check
//! (3-F) can run; 2-B owns it and its tests (`ReleaseCheckTests.OnlyDatedReleasesParse`,
//! `ReleasesCompareByTheirNumbers`).

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

/// A dated Octo release such as "2026.10.02" or "2026.10.02.1", compared by its numbers. The
/// repo also publishes "desktop-v1.3.2" and "android-v1.3.0"; those never parse, which is how
/// the server's own releases are told apart from the apps'.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ReleaseVersion {
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub patch: i32,
}

static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // .NET's `$` also matches before a final newline; the input is trimmed first, so no
    // newline can reach it.
    Regex::new(r"^v?([0-9]{4})\.([0-9]{2})\.([0-9]{2})(?:\.([0-9]{1,4}))?$").expect("a valid pattern")
});

impl ReleaseVersion {
    pub fn try_parse(text: Option<&str>) -> Option<ReleaseVersion> {
        // .NET stamps "+<commit>" after the informational version; the release is what is before it.
        let text = text.unwrap_or("");
        let head = text.split('+').next().unwrap_or("").trim();
        let caps = PATTERN.captures(head)?;
        let number = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<i32>().ok());
        Some(ReleaseVersion {
            year: number(1)?,
            month: number(2)?,
            day: number(3)?,
            patch: number(4).unwrap_or(0),
        })
    }

    /// The release this build came from, e.g. "2026.10.02", with the commit cut off.
    pub fn running() -> &'static str {
        crate::VERSION.split('+').next().unwrap_or(crate::VERSION)
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.patch > 0 {
            write!(
                f,
                "{:04}.{:02}.{:02}.{}",
                self.year, self.month, self.day, self.patch
            )
        } else {
            write!(f, "{:04}.{:02}.{:02}", self.year, self.month, self.day)
        }
    }
}
