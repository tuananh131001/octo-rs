//! The few .NET text rules the cover code leans on, spelled out so the Rust reads the same:
//! `Rune.IsLetter` (Unicode general category L*), `Rune.IsWhiteSpace`, ordinal ignore-case
//! comparison (simple upper-case mapping per character) and lengths in UTF-16 units.

use unicode_general_category::{GeneralCategory, get_general_category};

/// `Rune.IsLetter`: Lu, Ll, Lt, Lm or Lo.
pub(crate) fn is_letter(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

/// `Rune.IsWhiteSpace` (the same set as Rust's `char::is_whitespace`).
pub(crate) fn is_white_space(c: char) -> bool {
    c.is_whitespace()
}

/// .NET's invariant `ToUpper` of one character: the simple (one to one) mapping, so a letter
/// whose upper case is two letters (ß) stays as it is.
pub(crate) fn upper_simple(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)`.
pub(crate) fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars().count() == b.chars().count()
        && a.chars()
            .zip(b.chars())
            .all(|(x, y)| x == y || upper_simple(x) == upper_simple(y))
}

/// `text.EndsWith(suffix, StringComparison.OrdinalIgnoreCase)`, giving the text before the
/// suffix when it does.
pub(crate) fn strip_suffix_ignore_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let count = suffix.chars().count();
    let start = text.char_indices().rev().nth(count.checked_sub(1)?)?.0;
    eq_ignore_case(&text[start..], suffix).then(|| &text[..start])
}

/// `string.Length`: the text's length in UTF-16 code units.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_compare_ignoring_case() {
        assert_eq!(strip_suffix_ignore_case("Rock MIX", " Mix"), Some("Rock"));
        assert_eq!(strip_suffix_ignore_case("Rock Mixes", " Mix"), None);
        assert_eq!(strip_suffix_ignore_case("Mix", " Mix"), None);
        assert_eq!(strip_suffix_ignore_case(" Mix", " Mix"), Some(""));
        assert_eq!(strip_suffix_ignore_case("宇多田 Radio", " radio"), Some("宇多田"));
        assert!(is_letter('宇') && is_letter('é') && !is_letter('1') && !is_letter('🌙'));
        assert_eq!(utf16_len("a🌙"), 3);
    }
}
