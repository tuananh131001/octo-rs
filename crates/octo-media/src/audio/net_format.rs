//! The few pieces of .NET number handling these tools depend on: `Math.Round` (which rounds
//! half to even), the custom numeric format strings (`"0.0"`, `"0.###"`, `"0.000000"`) with
//! the invariant culture, and `Path.GetExtension`.

/// `Math.Round(value)`: half to even, as .NET's default `MidpointRounding.ToEven` does.
pub(crate) fn round(value: f64) -> f64 {
    value.round_ties_even()
}

/// `Math.Round(value, digits)`, the way .NET computes it: scale by a power of ten, round half
/// to even, scale back. Values too big to carry a fraction come back unchanged.
pub(crate) fn round_digits(value: f64, digits: i32) -> f64 {
    if value.abs() < 1e16 {
        let power10 = 10f64.powi(digits);
        round(value * power10) / power10
    } else {
        value
    }
}

/// `Math.Max(double, double)`: NaN wins, where Rust's `f64::max` ignores it.
pub(crate) fn max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// `value.ToString(format, CultureInfo.InvariantCulture)` for a custom format of the shape
/// `0.00##`: at least `min_decimals` and at most `max_decimals` digits after the point.
///
/// .NET takes the value to 15 significant digits first and then rounds that digit string half
/// away from zero at the last place shown, so `16.95` (really 16.9499999…) shows as `17.0`
/// with `"0.0"`, where Rust's own `{:.1}` would show `16.9`. A negative value that rounds to
/// zero loses its sign.
pub(crate) fn fixed(value: f64, min_decimals: usize, max_decimals: usize) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }

    // The 15 significant digits, and where the decimal point sits among them.
    let (mut digits, mut point): (Vec<u8>, i64) = if value == 0.0 {
        (Vec::new(), 0)
    } else {
        let scientific = format!("{:.14e}", value.abs());
        let (mantissa, exponent) = scientific
            .split_once('e')
            .expect("{:e} always writes an exponent");
        let exponent: i64 = exponent.parse().expect("{:e} writes an integer exponent");
        let digits = mantissa
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(|b| b - b'0')
            .collect();
        (digits, exponent + 1)
    };

    // Round half up at the last decimal shown.
    let keep = point + max_decimals as i64;
    if keep < 0 {
        digits.clear();
    } else if (keep as usize) < digits.len() {
        let round_up = digits[keep as usize] >= 5;
        digits.truncate(keep as usize);
        if round_up {
            let mut i = digits.len();
            loop {
                if i == 0 {
                    digits.insert(0, 1);
                    point += 1;
                    break;
                }
                i -= 1;
                if digits[i] == 9 {
                    digits[i] = 0;
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let is_zero = digits.is_empty();

    let digit_at = |index: i64| -> char {
        if index >= 0 && (index as usize) < digits.len() {
            (b'0' + digits[index as usize]) as char
        } else {
            '0'
        }
    };

    let mut text = String::new();
    if value < 0.0 && !is_zero {
        text.push('-');
    }
    if point <= 0 || is_zero {
        text.push('0');
    } else {
        for index in 0..point {
            text.push(digit_at(index));
        }
    }
    let mut fraction: String = (0..max_decimals as i64)
        .map(|offset| if is_zero { '0' } else { digit_at(point + offset) })
        .collect();
    while fraction.len() > min_decimals && fraction.ends_with('0') {
        fraction.pop();
    }
    if !fraction.is_empty() {
        text.push('.');
        text.push_str(&fraction);
    }
    text
}

/// `Path.GetExtension` on Linux: from the last dot of the file name, dot included, or empty
/// when there is no dot or it is the last character. `".flac"` alone is an extension here,
/// where `std::path::Path::extension` calls it a file stem.
pub(crate) fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(index) if index + 1 < name.len() => &name[index..],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_is_half_to_even() {
        for (value, expected) in [(0.5, 0.0), (1.5, 2.0), (2.5, 2.0), (-2.5, -2.0), (253.14, 253.0)] {
            assert_eq!(round(value), expected, "Math.Round({value})");
        }
        assert_eq!(round_digits(-6.5, 2), -6.5);
        assert_eq!(round_digits(0.125, 2), 0.12);
    }

    #[test]
    fn fixed_matches_dotnet_custom_formats() {
        let cases: &[(f64, usize, usize, &str)] = &[
            (16.95, 1, 1, "17.0"),
            (16.929, 1, 1, "16.9"),
            (0.0, 1, 1, "0.0"),
            (20.35, 1, 1, "20.4"),
            (42.5, 0, 0, "43"),
            (-0.3, 0, 0, "0"),
            (-1.5, 0, 0, "-2"),
            (1.7999999999999998, 0, 3, "1.8"),
            (30.0, 0, 3, "30"),
            (6.0, 0, 3, "6"),
            (0.0004, 0, 3, "0"),
            (12.3456, 0, 3, "12.346"),
            (0.966051, 6, 6, "0.966051"),
            (1.0, 6, 6, "1.000000"),
            (6.5, 2, 2, "6.50"),
            (99.995, 2, 2, "100.00"),
            (1e-5, 6, 6, "0.000010"),
        ];
        for &(value, min, max, expected) in cases {
            assert_eq!(
                fixed(value, min, max),
                expected,
                "{value} with {min}..{max} decimals"
            );
        }
    }

    #[test]
    fn get_extension_matches_dotnet() {
        let cases = [
            ("song.flac", ".flac"),
            ("/music/a.b/song", ""),
            ("song.", ""),
            (".flac", ".flac"),
            ("", ""),
            ("dir/SONG.FLAC", ".FLAC"),
        ];
        for (path, expected) in cases {
            assert_eq!(get_extension(path), expected, "Path.GetExtension({path:?})");
        }
    }
}
