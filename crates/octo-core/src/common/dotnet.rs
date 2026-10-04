//! The .NET string, character and number semantics the C# code leaned on, for the places where
//! Rust's own differ. Not a C# file: every port that called `char.IsLetter`,
//! `ToLowerInvariant`, `StringComparison.OrdinalIgnoreCase` or `Math.Round` uses these so the
//! answers stay the same.
//!
//! - **Character classes.** `char.IsLetter` and friends test the Unicode general category of
//!   one UTF-16 code unit, so a supplementary-plane letter (two surrogates) is never a letter to
//!   them. Rust's `char::is_alphabetic` tests the Alphabetic property instead, which also takes
//!   letter numbers and many marks. The `*_utf16` functions answer as `char.IsX` did; the plain
//!   ones answer as `Rune.IsX` did, on the whole code point.
//! - **Casing.** .NET maps one character to one character (simple case mapping), does not
//!   apply the final-sigma rule, and on ICU leaves U+0130 (İ) and U+0131 (ı) alone. Rust's
//!   `str::to_lowercase` does all three differently. `OrdinalIgnoreCase` has its own table.
//!   These were checked against .NET 9 for every cased code point.
//! - **Whitespace.** `char.IsWhiteSpace` and Rust's `char::is_whitespace` agree (the Unicode
//!   White_Space set), so `String.Trim()` is `str::trim` and needs nothing here.

use std::borrow::Cow;

use unicode_general_category::{GeneralCategory, get_general_category};

/// Whether the code point is a letter (Lu, Ll, Lt, Lm, Lo): `Rune.IsLetter`.
pub fn is_letter(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

/// Whether the code point is a decimal digit (Nd): `Rune.IsDigit`.
pub fn is_digit(c: char) -> bool {
    get_general_category(c) == GeneralCategory::DecimalNumber
}

/// `Rune.IsLetterOrDigit`: a letter or a decimal digit.
pub fn is_letter_or_digit(c: char) -> bool {
    is_letter(c) || is_digit(c)
}

/// A nonspacing (Mn) or spacing combining (Mc) mark.
pub fn is_combining_mark(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark
    )
}

/// Whether the code point is an uppercase letter (Lu): `Rune.IsUpper`.
pub fn is_upper(c: char) -> bool {
    get_general_category(c) == GeneralCategory::UppercaseLetter
}

/// Whether the code point is a lowercase letter (Ll): `Rune.IsLower`.
pub fn is_lower(c: char) -> bool {
    get_general_category(c) == GeneralCategory::LowercaseLetter
}

/// `char.IsLetter` on a UTF-16 code unit: a supplementary-plane character arrives as two
/// surrogates, and neither is a letter.
pub fn is_letter_utf16(c: char) -> bool {
    is_bmp(c) && is_letter(c)
}

/// `char.IsDigit` on a UTF-16 code unit.
pub fn is_digit_utf16(c: char) -> bool {
    is_bmp(c) && is_digit(c)
}

/// `char.IsUpper` on a UTF-16 code unit.
pub fn is_upper_utf16(c: char) -> bool {
    is_bmp(c) && is_upper(c)
}

/// `char.IsLower` on a UTF-16 code unit (and what a `\p{Ll}` regex class matched in .NET,
/// which also tested one code unit).
pub fn is_lower_utf16(c: char) -> bool {
    is_bmp(c) && is_lower(c)
}

fn is_bmp(c: char) -> bool {
    (c as u32) <= 0xFFFF
}

/// A cased letter (Lu, Ll, Lt) in one UTF-16 code unit: what .NET's `\p{Ll}` (or `\p{Lu}`)
/// matches under `RegexOptions.IgnoreCase`, which widens one case category to all three.
pub fn is_cased_letter_utf16(c: char) -> bool {
    is_bmp(c)
        && matches!(
            get_general_category(c),
            GeneralCategory::UppercaseLetter
                | GeneralCategory::LowercaseLetter
                | GeneralCategory::TitlecaseLetter
        )
}

/// Whether .NET's `\b` counts the character as a word character: a letter, nonspacing mark,
/// decimal digit or connector in one UTF-16 code unit, or a zero-width (non-)joiner. Rust's
/// `\w` also takes spacing and enclosing marks, letter numbers, other alphabetic symbols and
/// every supplementary-plane letter.
pub fn is_word_char(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    if c == '\u{200C}' || c == '\u{200D}' {
        return true;
    }
    is_bmp(c)
        && matches!(
            get_general_category(c),
            GeneralCategory::UppercaseLetter
                | GeneralCategory::LowercaseLetter
                | GeneralCategory::TitlecaseLetter
                | GeneralCategory::ModifierLetter
                | GeneralCategory::OtherLetter
                | GeneralCategory::NonspacingMark
                | GeneralCategory::DecimalNumber
                | GeneralCategory::ConnectorPunctuation
        )
}

/// A punctuation or symbol character of the same UTF-8 length, that no Rust class used here
/// (`\w`, `\d`, `\s`, `\p{L}`, `\p{M}`) matches.
fn placeholder(c: char) -> char {
    match c.len_utf8() {
        2 => '\u{00A1}',
        3 => '\u{3003}',
        _ => '\u{1F600}',
    }
}

fn view(text: &str, swap: impl Fn(char) -> bool) -> Cow<'_, str> {
    if !text.chars().any(&swap) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| if swap(c) { placeholder(c) } else { c })
            .collect(),
    )
}

/// The text as a Rust regex with `\b` must see it to find what the .NET regex found: every
/// character .NET did not count as a word character, but Rust might, swapped for a placeholder
/// of the same UTF-8 length. Byte offsets into the view are offsets into the text, so match
/// there and slice the original.
pub fn word_boundary_view(text: &str) -> Cow<'_, str> {
    view(text, |c| !c.is_ascii() && !c.is_whitespace() && !is_word_char(c))
}

/// The text as a Rust regex with `\d`, `\p{L}` or `\p{M}` must see it to find what the .NET
/// regex found: .NET tested UTF-16 code units, so a supplementary-plane digit or letter
/// matched none of them. Those are swapped for a placeholder of the same UTF-8 length.
pub fn utf16_class_view(text: &str) -> Cow<'_, str> {
    view(text, |c| !is_bmp(c))
}

/// `string.IsNullOrWhiteSpace` for a value that is present.
pub fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// `string.IsNullOrWhiteSpace`.
pub fn is_null_or_white_space(s: Option<&str>) -> bool {
    s.is_none_or(is_blank)
}

/// The length .NET reports for the string: UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `char.ToLowerInvariant`: the simple lowercase mapping, one character for one. U+0130 (İ)
/// stays as it is: .NET on ICU special-cases it rather than lowercasing it to an ASCII `i`.
pub fn to_lower_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    // Rust's full mapping differs from the simple one only for U+0130 ("i̇"), which .NET keeps.
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// `char.ToUpperInvariant`: the simple uppercase mapping, one character for one. U+0131 (ı)
/// stays as it is, as .NET on ICU keeps it; U+017F (ſ) does become `S`.
pub fn to_upper_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    if c == '\u{0131}' {
        return c;
    }
    if let Some(upper) = greek_ypogegrammeni_upper(c) {
        return upper;
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u,
        // A full mapping of several characters ("ß" → "SS") has no simple one in these cases.
        _ => c,
    }
}

/// The Greek letters with ypogegrammeni, whose full uppercase is two letters ("ᾳ" → "ΑΙ") but
/// whose simple uppercase, the one .NET uses, is the titlecase letter with prosgegrammeni.
fn greek_ypogegrammeni_upper(c: char) -> Option<char> {
    let code = c as u32;
    let upper = match code {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => code + 8,
        0x1FB3 => 0x1FBC,
        0x1FC3 => 0x1FCC,
        0x1FF3 => 0x1FFC,
        _ => return None,
    };
    char::from_u32(upper)
}

/// `string.ToLowerInvariant`.
pub fn to_lower_invariant(s: &str) -> String {
    s.chars().map(to_lower_char).collect()
}

/// `string.ToUpperInvariant`.
pub fn to_upper_invariant(s: &str) -> String {
    s.chars().map(to_upper_char).collect()
}

/// What `StringComparison.OrdinalIgnoreCase` compares a character as: its simple uppercase,
/// except that ı and ſ never meet ASCII I and S (where `ToUpperInvariant` does take ſ to S).
fn ordinal_case_key(c: char) -> char {
    if c == '\u{0131}' || c == '\u{017F}' {
        return c;
    }
    to_upper_char(c)
}

/// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)`.
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    let mut left = a.chars();
    let mut right = b.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y || ordinal_case_key(x) == ordinal_case_key(y) => {}
            _ => return false,
        }
    }
}

/// A key under which strings that are equal by `StringComparer.OrdinalIgnoreCase` are equal,
/// for a dictionary built with that comparer.
pub fn ordinal_ignore_case_key(s: &str) -> String {
    s.chars().map(ordinal_case_key).collect()
}

/// `string.Compare(a, b, StringComparison.OrdinalIgnoreCase)`: the case-mapped UTF-16 code
/// units compared one by one, so a supplementary-plane character sorts by its surrogates.
pub fn compare_ordinal_ignore_case(a: &str, b: &str) -> std::cmp::Ordering {
    let units = |s: &str| -> Vec<u16> {
        let mut out = Vec::with_capacity(s.len());
        let mut buf = [0u16; 2];
        for c in s.chars() {
            out.extend_from_slice(ordinal_case_key(c).encode_utf16(&mut buf));
        }
        out
    };
    units(a).cmp(&units(b))
}

/// `s.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)`.
pub fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    let mut chars = s.chars();
    for p in prefix.chars() {
        match chars.next() {
            Some(c) if c == p || ordinal_case_key(c) == ordinal_case_key(p) => {}
            _ => return false,
        }
    }
    true
}

/// `Math.Round(value, digits)`: half to even at the given number of decimals, computed as .NET
/// does (scale, round, unscale) so the last bit agrees.
pub fn round(value: f64, digits: u32) -> f64 {
    const ROUND_LIMIT: f64 = 1e16;
    if value.abs() >= ROUND_LIMIT {
        return value;
    }
    let power10 = 10f64.powi(digits as i32);
    (value * power10).round_ties_even() / power10
}

/// A double written with the custom format `"0.#"` (`decimals` = 1; `"0.##"` is 2, and so on):
/// .NET first reads the value to 15 significant digits, rounds those half away from zero at
/// the last place shown, and drops trailing zeros and a bare point. A negative value keeps its
/// sign even when it rounds to zero (`"-0"`), as .NET Core 3.0 and later write it.
pub fn format_optional_decimals(value: f64, decimals: u32) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let sign = if value.is_sign_negative() && value != 0.0 {
        "-"
    } else {
        ""
    };
    // 15 significant digits: value = digits * 10^(exponent - 14).
    let sci = format!("{:.14e}", value.abs());
    let (mantissa, exponent) = sci.split_once('e').expect("scientific notation has an exponent");
    let exponent: i32 = exponent.parse().expect("the exponent is a number");
    let digits: u128 = mantissa.replace('.', "").parse().expect("the mantissa is digits");
    // The value times 10^decimals, as an integer rounded half away from zero.
    let shift = exponent - 14 + decimals as i32;
    let scaled = if shift >= 0 {
        match 10u128
            .checked_pow(shift as u32)
            .and_then(|p| digits.checked_mul(p))
        {
            Some(scaled) => scaled,
            None => return format!("{sign}{}", value.abs()),
        }
    } else if shift < -38 {
        0
    } else {
        let divisor = 10u128.pow((-shift) as u32);
        let (quotient, remainder) = (digits / divisor, digits % divisor);
        if remainder * 2 >= divisor {
            quotient + 1
        } else {
            quotient
        }
    };
    let unit = 10u128.pow(decimals);
    let mut out = format!("{sign}{}", scaled / unit);
    if decimals > 0 {
        let fraction = format!("{:0width$}", scaled % unit, width = decimals as usize);
        let fraction = fraction.trim_end_matches('0');
        if !fraction.is_empty() {
            out.push('.');
            out.push_str(fraction);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinal_ignore_case_keys_and_ordering() {
        assert_eq!(
            ordinal_ignore_case_key("Peer1|a\\B.flac"),
            ordinal_ignore_case_key("PEER1|A\\b.FLAC")
        );
        assert_ne!(ordinal_ignore_case_key("ı"), ordinal_ignore_case_key("I"));
        use std::cmp::Ordering;
        assert_eq!(compare_ordinal_ignore_case("albums", "Beta"), Ordering::Less);
        assert_eq!(compare_ordinal_ignore_case("ABC", "abc"), Ordering::Equal);
        // Upper-cased, '_' (0x5F) sorts after the letters (0x41..0x5A).
        assert_eq!(compare_ordinal_ignore_case("_x", "z"), Ordering::Greater);
        // A surrogate (0xD800..) sorts before U+FF21 by UTF-16 units, though after it by code point.
        assert_eq!(
            compare_ordinal_ignore_case("\u{1F3B5}", "\u{FF21}"),
            Ordering::Less
        );
    }

    #[test]
    fn character_classes_answer_as_dotnet_did() {
        assert!(is_letter('é') && is_letter('ß') && is_letter('漢') && is_letter('ー'));
        // A letter number and a circled letter are Alphabetic to Rust, not letters to .NET.
        assert!(!is_letter('Ⅻ') && 'Ⅻ'.is_alphabetic());
        assert!(!is_letter('ⓐ') && 'ⓐ'.is_alphabetic());
        // A supplementary letter is a letter as a rune, two surrogates as UTF-16 chars.
        assert!(is_letter('𠀀') && !is_letter_utf16('𠀀'));
        // Superscripts are numeric to Rust but not decimal digits.
        assert!(!is_digit('²') && '²'.is_numeric());
        assert!(is_digit('٣'));
        assert!(is_upper('A') && !is_upper('Ⓐ'));
    }

    #[test]
    fn casing_is_simple_with_dotnets_exceptions() {
        // Each answer below is what .NET 9 on ICU gave.
        assert_eq!(to_lower_invariant("İSTANBUL"), "İstanbul");
        assert_eq!(to_upper_invariant("ıstanbul"), "ıSTANBUL");
        assert_eq!(to_upper_invariant("ſ"), "S");
        assert_eq!(to_lower_invariant("\u{212A}"), "k");
        assert_eq!(to_upper_invariant("straße"), "STRAßE");
        assert_eq!(to_lower_invariant("ẞ"), "ß");
        assert_eq!(to_upper_invariant("µ"), "Μ");
        // No final sigma.
        assert_eq!(to_lower_invariant("ΟΔΟΣ"), "οδοσ");
        assert_eq!(to_upper_invariant("ᾳ"), "ᾼ");
        assert_eq!(to_lower_invariant("𐐀"), "𐐨");
        assert!(eq_ignore_case("Ärger", "äRGER"));
        assert!(eq_ignore_case("ǅ", "ǆ"));
        assert!(eq_ignore_case("µ", "Μ"));
        assert!(!eq_ignore_case("ı", "I"));
        assert!(!eq_ignore_case("ſ", "s"));
        assert!(!eq_ignore_case("\u{212A}", "k"));
        assert!(!eq_ignore_case("ẞ", "ß"));
        assert!(starts_with_ignore_case("The Beatles", "the "));
        assert!(!starts_with_ignore_case("Th", "the "));
    }

    #[test]
    fn views_keep_byte_offsets_and_hide_what_dotnet_did_not_see() {
        // A spacing mark after "live" ends the word for .NET.
        let text = "live\u{093E} 𠀀 é ۵";
        let view = word_boundary_view(text);
        assert_eq!(view.len(), text.len());
        assert_eq!(view, "live\u{3003} \u{1F600} é ۵");
        assert!(matches!(word_boundary_view("plain é"), Cow::Borrowed(_)));
        assert_eq!(utf16_class_view("1𑁦2"), "1\u{1F600}2");
        assert!(is_word_char('\u{200D}') && is_word_char('é') && !is_word_char('\u{093E}'));
        assert!(is_cased_letter_utf16('E') && is_cased_letter_utf16('ǅ') && !is_cased_letter_utf16('侃'));
    }

    #[test]
    fn round_is_half_to_even() {
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(0.135, 2), 0.14);
        assert_eq!(round(1.0 - 0.15 - 0.1, 2), 0.75);
        assert_eq!(round(2.5, 0), 2.0);
    }

    #[test]
    fn optional_decimals_round_away_from_zero_on_fifteen_digits() {
        let cases: &[(f64, &str)] = &[
            (7.0, "7"),
            (4.5, "4.5"),
            (4.25, "4.3"),
            (4.35, "4.4"),
            (10.0, "10"),
            (9.96, "10"),
            (0.04, "0"),
            (0.05, "0.1"),
            (123.456, "123.5"),
            (0.0, "0"),
            (-1.25, "-1.3"),
            (-0.04, "-0"),
            (1e20, "100000000000000000000"),
        ];
        for &(value, want) in cases {
            assert_eq!(format_optional_decimals(value, 1), want, "{value}");
        }
    }
}
