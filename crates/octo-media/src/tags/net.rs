//! The .NET number parsing TagLib# relies on when it reads a tag, with the invariant culture:
//! `byte.TryParse`, `uint.TryParse`, `int.TryParse` (`NumberStyles.Integer`: surrounding white
//! space and a leading sign allowed) and `double.TryParse`.

/// The digits of an integer as `NumberStyles.Integer` accepts them: white space around, an
/// optional sign, then ASCII digits only. Returns the sign and the digits.
fn integer_parts(text: &str) -> Option<(bool, &str)> {
    let text = text.trim_matches(is_number_white_space);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        _ => (false, text),
    };
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some((negative, digits))
}

/// `NumberStyles.AllowLeadingWhite | AllowTrailingWhite` accept only these characters.
fn is_number_white_space(c: char) -> bool {
    matches!(c, '\u{09}'..='\u{0D}' | ' ')
}

/// An unsigned value with the given maximum; "-0" is zero, as .NET reads it.
fn parse_unsigned(text: &str, max: u64) -> Option<u64> {
    let (negative, digits) = integer_parts(text)?;
    let digits = digits.trim_start_matches('0');
    if digits.len() > 20 {
        return None;
    }
    let value: u64 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    match (negative, value) {
        (true, 0) => Some(0),
        (true, _) => None,
        (false, value) if value <= max => Some(value),
        _ => None,
    }
}

/// `byte.TryParse(text, out value)`.
pub(crate) fn parse_byte(text: &str) -> Option<u8> {
    parse_unsigned(text, u8::MAX as u64).map(|value| value as u8)
}

/// `uint.TryParse(text, out value)`.
pub(crate) fn parse_uint(text: &str) -> Option<u32> {
    parse_unsigned(text, u32::MAX as u64).map(|value| value as u32)
}

/// `int.TryParse(text, out value)`.
pub(crate) fn parse_int(text: &str) -> Option<i32> {
    let (negative, digits) = integer_parts(text)?;
    let digits = digits.trim_start_matches('0');
    if digits.len() > 11 {
        return None;
    }
    let magnitude: i64 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let value = if negative { -magnitude } else { magnitude };
    i32::try_from(value).ok()
}

/// `double.TryParse(text, out value)` with the invariant culture (`NumberStyles.Float |
/// AllowThousands`): white space, a sign, digits with group commas before the point, a
/// fraction, an exponent. "NaN" and "Infinity" are numbers too.
pub(crate) fn parse_double(text: &str) -> Option<f64> {
    let text = text.trim_matches(is_number_white_space);
    match text {
        "NaN" => return Some(f64::NAN),
        "Infinity" | "+Infinity" | "∞" => return Some(f64::INFINITY),
        "-Infinity" | "-∞" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'+') => ("", &text[1..]),
        Some(b'-') => ("-", &text[1..]),
        _ => ("", text),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    // Group separators are allowed anywhere in the integral part, though not first.
    if whole.starts_with(',') || !whole.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        return None;
    }
    let whole: String = whole.chars().filter(|c| *c != ',').collect();
    if let Some(fraction) = fraction
        && !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if whole.is_empty() && fraction.is_none_or(str::is_empty) {
        return None;
    }
    let mut literal = format!(
        "{sign}{}.{}",
        if whole.is_empty() { "0" } else { &whole },
        fraction.unwrap_or("0")
    );
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        literal.push('e');
        literal.push_str(exponent);
    }
    literal.parse().ok()
}

/// `double.TryParse(text, NumberStyles.AllowDecimalPoint, NumberFormatInfo.InvariantInfo)`:
/// digits and one point, nothing else (no sign, no white space).
pub(crate) fn parse_decimal_point_double(text: &str) -> Option<f64> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) {
        return None;
    }
    format!(
        "{}.{}",
        if whole.is_empty() { "0" } else { whole },
        if fraction.is_empty() { "0" } else { fraction }
    )
    .parse()
    .ok()
}

/// `(int)Math.Round(seconds)`: half to even, as TagLib#'s callers turn a duration into seconds.
pub(crate) fn round_seconds(seconds: f64) -> i32 {
    seconds.round_ties_even() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_parse_as_dotnet_parses_them() {
        assert_eq!(parse_byte("17"), Some(17));
        assert_eq!(parse_byte(" 17 "), Some(17));
        assert_eq!(parse_byte("+5"), Some(5));
        assert_eq!(parse_byte("-0"), Some(0));
        assert_eq!(parse_byte("-1"), None);
        assert_eq!(parse_byte("256"), None);
        assert_eq!(parse_byte("1 7"), None);
        assert_eq!(parse_byte("(17)"), None);
        assert_eq!(parse_byte(""), None);
        assert_eq!(parse_uint("03"), Some(3));
        assert_eq!(parse_uint("4294967295"), Some(u32::MAX));
        assert_eq!(parse_uint("4294967296"), None);
        assert_eq!(parse_int("-12"), Some(-12));
        assert_eq!(parse_int("2147483648"), None);
    }

    #[test]
    fn doubles_parse_as_dotnet_parses_them() {
        assert_eq!(parse_double("120"), Some(120.0));
        assert_eq!(parse_double(" 120.5 "), Some(120.5));
        assert_eq!(parse_double("1,200"), Some(1200.0));
        assert_eq!(parse_double(".5"), Some(0.5));
        assert_eq!(parse_double("5."), Some(5.0));
        assert_eq!(parse_double("1e2"), Some(100.0));
        assert_eq!(parse_double("abc"), None);
        assert_eq!(parse_double("."), None);
        assert_eq!(parse_decimal_point_double("120.5"), Some(120.5));
        assert_eq!(parse_decimal_point_double(" 120"), None);
        assert_eq!(parse_decimal_point_double("-1"), None);
    }

    #[test]
    fn seconds_round_half_to_even() {
        assert_eq!(round_seconds(0.25), 0);
        assert_eq!(round_seconds(0.5), 0);
        assert_eq!(round_seconds(1.5), 2);
        assert_eq!(round_seconds(2.5), 2);
    }
}
