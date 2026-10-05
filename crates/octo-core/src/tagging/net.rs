//! The few .NET string and number behaviours the tagging code leans on that
//! `common::dotnet` does not carry: `OrdinalIgnoreCase` keys for sets and groupings, ordinal
//! (UTF-16) ordering, `int.TryParse` and the custom `"0.00"` formats.

use std::cmp::Ordering;

use crate::common::dotnet::{format_optional_decimals, to_upper_char};

/// What `StringComparer.OrdinalIgnoreCase` hashes a string as: each character's simple
/// uppercase, except that ı and ſ keep themselves (they never meet ASCII I and S).
pub(crate) fn ignore_case_key(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\u{0131}' || c == '\u{017F}' {
                c
            } else {
                to_upper_char(c)
            }
        })
        .collect()
}

/// `StringComparer.Ordinal`: UTF-16 code unit order (which differs from Rust's code point
/// order only between supplementary characters and U+E000..U+FFFF).
pub(crate) fn cmp_ordinal(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `int.TryParse(s, out n)` with the invariant culture: optional leading and trailing white
/// space (tab to carriage return, and space), an optional sign, ASCII digits, within `i32`.
pub(crate) fn parse_int(s: &str) -> Option<i32> {
    let is_white = |c: char| c == ' ' || ('\u{9}'..='\u{D}').contains(&c);
    let trimmed = s.trim_matches(is_white);
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i64 = 0;
    for b in digits.bytes() {
        value = value * 10 + i64::from(b - b'0');
        if value > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).ok()
}

/// The first `units` UTF-16 code units of `s` (`s[..units]`), or None when the cut would split a
/// supplementary character (the result could then never parse as digits anyway).
pub(crate) fn utf16_prefix(s: &str, units: usize) -> Option<&str> {
    let mut taken = 0;
    for (at, c) in s.char_indices() {
        if taken == units {
            return Some(&s[..at]);
        }
        taken += c.len_utf16();
        if taken > units {
            return None;
        }
    }
    (taken == units).then_some(s)
}

/// `value.ToString("0.00", CultureInfo.InvariantCulture)` (`decimals` = 2; `"0.0"` is 1, `"0.000"`
/// is 3): exactly that many decimals, rounded half away from zero on 15 significant digits.
pub(crate) fn fixed(value: f64, decimals: u32) -> String {
    let mut text = format_optional_decimals(value, decimals);
    if decimals > 0 && value.is_finite() {
        let shown = text.split_once('.').map_or(0, |(_, fraction)| fraction.len());
        if shown == 0 {
            text.push('.');
        }
        for _ in shown..decimals as usize {
            text.push('0');
        }
    }
    text
}

/// `value.ToString("+0.00;-0.00", CultureInfo.InvariantCulture)`: the sign always shown. A
/// negative value that rounds to zero is written with the first section, as .NET does.
pub(crate) fn signed_fixed(value: f64, decimals: u32) -> String {
    let magnitude = fixed(value.abs(), decimals);
    let rounds_to_zero = magnitude.bytes().all(|b| b == b'0' || b == b'.');
    if value < 0.0 && !rounds_to_zero {
        format!("-{magnitude}")
    } else {
        format!("+{magnitude}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_reads_as_int_try_parse() {
        assert_eq!(parse_int("1998"), Some(1998));
        assert_eq!(parse_int(" 199"), Some(199));
        assert_eq!(parse_int("+12"), Some(12));
        assert_eq!(parse_int("-12"), Some(-12));
        assert_eq!(parse_int("A1"), None);
        assert_eq!(parse_int("19-0"), None);
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("-"), None);
        assert_eq!(parse_int("2147483647"), Some(i32::MAX));
        assert_eq!(parse_int("-2147483648"), Some(i32::MIN));
        assert_eq!(parse_int("2147483648"), None);
        assert_eq!(parse_int("\u{0663}"), None);
    }

    #[test]
    fn utf16_prefix_cuts_on_code_units() {
        assert_eq!(utf16_prefix("1998-04-20", 4), Some("1998"));
        assert_eq!(utf16_prefix("1998", 4), Some("1998"));
        assert_eq!(utf16_prefix("199", 4), None);
        assert_eq!(utf16_prefix("19\u{1F600}x", 3), None);
        assert_eq!(utf16_prefix("19\u{1F600}x", 4), Some("19\u{1F600}"));
    }

    #[test]
    fn fixed_formats_as_the_custom_formats() {
        assert_eq!(fixed(0.2, 2), "0.20");
        assert_eq!(fixed(1.0, 2), "1.00");
        assert_eq!(fixed(0.125, 2), "0.13");
        assert_eq!(fixed(0.0, 3), "0.000");
        assert_eq!(fixed(12.34, 1), "12.3");
        assert_eq!(fixed(16.95, 1), "17.0");
        assert_eq!(signed_fixed(-6.5, 2), "-6.50");
        assert_eq!(signed_fixed(6.5, 2), "+6.50");
        assert_eq!(signed_fixed(0.0, 2), "+0.00");
        assert_eq!(signed_fixed(-0.001, 2), "+0.00");
    }

    #[test]
    fn ignore_case_key_and_ordinal_order() {
        assert_eq!(ignore_case_key("rec-Ab"), ignore_case_key("REC-aB"));
        assert_ne!(ignore_case_key("ı"), ignore_case_key("I"));
        assert_eq!(cmp_ordinal("1998", "1999"), Ordering::Less);
        assert_eq!(cmp_ordinal("\u{1F600}", "\u{FF21}"), Ordering::Less);
    }
}
