//! Port of `Services/Updates/ReleaseVersion.cs`.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

/// `^v?(\d{4})\.(\d{2})\.(\d{2})(?:\.(\d{1,4}))?$`, with ASCII digits: .NET's `\d` also took
/// other scripts' digits, on which `int.Parse` then threw (known-diffs.md).
static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^v?([0-9]{4})\.([0-9]{2})\.([0-9]{2})(?:\.([0-9]{1,4}))?$")
        .expect("a fixed pattern compiles")
});

/// A dated Octo release such as "2026.10.02" or "2026.10.02.1", compared by its numbers. The
/// repo also publishes "desktop-v1.3.2" and "android-v1.3.0"; those never parse, which is how
/// the server's own releases are told apart from the apps'.
///
/// Ordered by year, month, day, then patch (the field order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ReleaseVersion {
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub patch: i32,
}

impl ReleaseVersion {
    pub fn new(year: i32, month: i32, day: i32, patch: i32) -> Self {
        Self {
            year,
            month,
            day,
            patch,
        }
    }

    /// `TryParse`. Pass "" for a null text.
    pub fn try_parse(text: &str) -> Option<Self> {
        // .NET stamps "+<commit>" after the informational version; the release is what is before it.
        let release = text.split('+').next().unwrap_or("").trim();
        let caps = PATTERN.captures(release)?;
        let number = |index: usize| -> Option<i32> { caps.get(index)?.as_str().parse().ok() };
        Some(Self::new(
            number(1)?,
            number(2)?,
            number(3)?,
            if caps.get(4).is_some() { number(4)? } else { 0 },
        ))
    }

    /// The release this build came from, e.g. "2026.10.02", with the commit cut off.
    pub fn running() -> &'static str {
        crate::VERSION.split('+').next().unwrap_or(crate::VERSION)
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}.{:02}.{:02}", self.year, self.month, self.day)?;
        if self.patch > 0 {
            write!(f, ".{}", self.patch)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ReleaseCheckTests.OnlyDatedReleasesParse.
    #[test]
    fn only_dated_releases_parse() {
        for (text, parses) in [
            ("2026.10.02", true),
            ("2026.10.02.1", true),
            ("v2026.10.02", true),
            ("2026.10.02+ac471ba9406dffe65fe784a94bea5d863622f145", true),
            ("desktop-v1.3.2", false),
            ("android-v1.3.0", false),
            ("1.0.0", false),
            ("", false),
        ] {
            assert_eq!(ReleaseVersion::try_parse(text).is_some(), parses, "{text}");
        }
    }

    /// ReleaseCheckTests.ReleasesCompareByTheirNumbers.
    #[test]
    fn releases_compare_by_their_numbers() {
        for (newer, older) in [
            ("2026.10.02.1", "2026.10.02"),
            ("2026.10.10", "2026.10.09"),
            ("2026.11.01", "2026.10.31"),
            ("2027.01.01", "2026.12.31.9"),
        ] {
            let a = ReleaseVersion::try_parse(newer).expect("parses");
            let b = ReleaseVersion::try_parse(older).expect("parses");
            assert!(a > b, "{newer} > {older}");
            assert!(b < a, "{older} < {newer}");
        }
    }

    #[test]
    fn a_release_writes_its_patch_only_when_it_has_one() {
        let parse = |text| ReleaseVersion::try_parse(text).expect("parses").to_string();
        assert_eq!(parse(" v2026.10.02.0 "), "2026.10.02");
        assert_eq!(parse("2026.10.02.12+abc"), "2026.10.02.12");
        assert_eq!(ReleaseVersion::new(26, 1, 2, 0).to_string(), "0026.01.02");
    }

    #[test]
    fn the_running_release_has_no_commit() {
        assert!(!ReleaseVersion::running().contains('+'));
        assert!(ReleaseVersion::try_parse(ReleaseVersion::running()).is_some());
    }

    #[test]
    fn other_scripts_digits_are_not_a_release() {
        assert!(ReleaseVersion::try_parse("٢٠٢٦.١٠.٠٢").is_none());
    }
}
