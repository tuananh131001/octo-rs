//! A serde_json formatter that writes what System.Text.Json writes.
//!
//! Octo's state files and API answers were written by System.Text.Json, and existing
//! installs carry those files. Writing them back byte for byte the same means matching three
//! things serde_json does differently by default:
//!
//! - **Escaping.** `JavaScriptEncoder.Default` escapes every non-ASCII character as `\uXXXX`
//!   with uppercase hex (astral characters as a surrogate pair), and also `"` `&` `'` `+`
//!   `<` `>` `` ` `` and DEL. `UnsafeRelaxedJsonEscaping` leaves non-ASCII and the HTML
//!   characters alone but still escapes controls, DEL, U+2028/2029 and astral characters.
//! - **Doubles.** .NET writes the shortest round-trip digits, `1` rather than `1.0`, and
//!   switches to `1E-05` / `1.5E+300` outside a fixed range; see [`format_double`].
//! - **Indentation.** Two spaces and `"key": value`, which matches serde_json's pretty
//!   printer; it is reimplemented here only because one formatter must do both.

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter};
use std::io::{self, Write};

/// Which System.Text.Json encoder to imitate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Escaping {
    /// `JavaScriptEncoder.Default`: everything outside printable ASCII, plus the
    /// HTML-sensitive characters. What every Octo state file and API answer used.
    #[default]
    Default,
    /// `JavaScriptEncoder.UnsafeRelaxedJsonEscaping`.
    Relaxed,
}

/// Output options: the encoder and whether to indent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub escaping: Escaping,
    pub indented: bool,
}

impl Options {
    pub const COMPACT: Options = Options {
        escaping: Escaping::Default,
        indented: false,
    };
    pub const INDENTED: Options = Options {
        escaping: Escaping::Default,
        indented: true,
    };
}

/// Serializes as System.Text.Json would with the default encoder, compact.
pub fn to_string<T: Serialize + ?Sized>(value: &T) -> String {
    to_string_with(value, Options::COMPACT)
}

/// Serializes as System.Text.Json would with `WriteIndented = true`.
pub fn to_string_indented<T: Serialize + ?Sized>(value: &T) -> String {
    to_string_with(value, Options::INDENTED)
}

pub fn to_string_with<T: Serialize + ?Sized>(value: &T, options: Options) -> String {
    String::from_utf8(to_vec_with(value, options)).expect("the formatter writes UTF-8")
}

pub fn to_vec_with<T: Serialize + ?Sized>(value: &T, options: Options) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, StjFormatter::new(options));
    // Serializing to memory only fails for a type whose Serialize impl fails, such as a map
    // with non-string keys, which is a programming error.
    value.serialize(&mut ser).expect("value serializes to JSON");
    out
}

/// The formatter itself, for callers that drive serde_json directly.
#[derive(Debug, Clone)]
pub struct StjFormatter {
    escaping: Escaping,
    indented: bool,
    depth: usize,
    has_value: bool,
}

impl StjFormatter {
    pub fn new(options: Options) -> Self {
        StjFormatter {
            escaping: options.escaping,
            indented: options.indented,
            depth: 0,
            has_value: false,
        }
    }

    fn newline_indent<W: ?Sized + Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b"\n")?;
        for _ in 0..self.depth {
            writer.write_all(b"  ")?;
        }
        Ok(())
    }
}

/// Writes `text` with the escapes the chosen encoder applies, without surrounding quotes.
pub fn write_escaped<W: ?Sized + Write>(writer: &mut W, text: &str, escaping: Escaping) -> io::Result<()> {
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if needs_escape(c, escaping) {
            if start < i {
                writer.write_all(&text.as_bytes()[start..i])?;
            }
            write_char_escaped(writer, c, escaping)?;
            start = i + c.len_utf8();
        }
    }
    if start < text.len() {
        writer.write_all(&text.as_bytes()[start..])?;
    }
    Ok(())
}

/// `text` as a quoted JSON string with the chosen encoder's escapes.
pub fn quote(text: &str, escaping: Escaping) -> String {
    let mut out = Vec::with_capacity(text.len() + 2);
    out.push(b'"');
    write_escaped(&mut out, text, escaping).expect("writing to memory");
    out.push(b'"');
    String::from_utf8(out).expect("escaped output is UTF-8")
}

fn needs_escape(c: char, escaping: Escaping) -> bool {
    let n = c as u32;
    if n < 0x20 || c == '"' || c == '\\' || n == 0x7F {
        return true;
    }
    match escaping {
        Escaping::Default => n > 0x7E || matches!(c, '&' | '\'' | '+' | '<' | '>' | '`'),
        Escaping::Relaxed => {
            (0x80..=0x9F).contains(&n) || n == 0x2028 || n == 0x2029 || n > 0xFFFF || n == 0xFEFF
        }
    }
}

fn write_char_escaped<W: ?Sized + Write>(writer: &mut W, c: char, escaping: Escaping) -> io::Result<()> {
    match c {
        '\n' => writer.write_all(b"\\n"),
        '\r' => writer.write_all(b"\\r"),
        '\t' => writer.write_all(b"\\t"),
        '\u{8}' => writer.write_all(b"\\b"),
        '\u{c}' => writer.write_all(b"\\f"),
        '\\' => writer.write_all(b"\\\\"),
        '"' if escaping == Escaping::Relaxed => writer.write_all(b"\\\""),
        _ => {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                write!(writer, "\\u{:04X}", unit)?;
            }
            Ok(())
        }
    }
}

impl Formatter for StjFormatter {
    fn write_f64<W: ?Sized + Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        writer.write_all(format_double(value).as_bytes())
    }

    fn write_f32<W: ?Sized + Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        writer.write_all(format_single(value).as_bytes())
    }

    fn write_string_fragment<W: ?Sized + Write>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()> {
        write_escaped(writer, fragment, self.escaping)
    }

    fn write_char_escape<W: ?Sized + Write>(&mut self, writer: &mut W, escape: CharEscape) -> io::Result<()> {
        let c = match escape {
            CharEscape::Quote => '"',
            CharEscape::ReverseSolidus => '\\',
            CharEscape::Solidus => '/',
            CharEscape::Backspace => '\u{8}',
            CharEscape::FormFeed => '\u{c}',
            CharEscape::LineFeed => '\n',
            CharEscape::CarriageReturn => '\r',
            CharEscape::Tab => '\t',
            CharEscape::AsciiControl(b) => b as char,
        };
        if c == '/' {
            return writer.write_all(b"/");
        }
        write_char_escaped(writer, c, self.escaping)
    }

    fn begin_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(b"[")
    }

    fn end_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.depth -= 1;
        if self.indented && self.has_value {
            self.newline_indent(writer)?;
        }
        writer.write_all(b"]")
    }

    fn begin_array_value<W: ?Sized + Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
        if !first {
            writer.write_all(b",")?;
        }
        if self.indented {
            self.newline_indent(writer)?;
        }
        Ok(())
    }

    fn end_array_value<W: ?Sized + Write>(&mut self, _writer: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }

    fn begin_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(b"{")
    }

    fn end_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.depth -= 1;
        if self.indented && self.has_value {
            self.newline_indent(writer)?;
        }
        writer.write_all(b"}")
    }

    fn begin_object_key<W: ?Sized + Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
        if !first {
            writer.write_all(b",")?;
        }
        if self.indented {
            self.newline_indent(writer)?;
        }
        Ok(())
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(if self.indented { b": " } else { b":" })
    }

    fn end_object_value<W: ?Sized + Write>(&mut self, _writer: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
}

/// A double as .NET Core 3.0+ writes it (`double.ToString("R")`, which is also what
/// Utf8JsonWriter emits): the shortest digits that round-trip, in fixed notation while the
/// decimal point sits between 3 places left of the first digit and 17 places right of it,
/// otherwise `d.dddE+XX` with a signed, at least two-digit exponent.
pub fn format_double(value: f64) -> String {
    format_shortest(
        value.is_nan(),
        value.is_infinite(),
        value.is_sign_negative(),
        &format!("{:e}", value),
        17,
    )
}

/// A float as .NET writes `float.ToString("R")`: as [`format_double`], with the switch to
/// exponent notation past 9 digits.
pub fn format_single(value: f32) -> String {
    format_shortest(
        value.is_nan(),
        value.is_infinite(),
        value.is_sign_negative(),
        &format!("{:e}", value),
        9,
    )
}

fn format_shortest(nan: bool, infinite: bool, negative: bool, sci: &str, max_scale: i32) -> String {
    if nan {
        return "NaN".into();
    }
    if infinite {
        return if negative {
            "-Infinity".into()
        } else {
            "Infinity".into()
        };
    }
    // Rust's `{:e}` gives the shortest round-trip digits: "-1.2345e17", "1e-5", "0e0".
    let (mantissa, exp) = sci.split_once('e').expect("scientific notation has an exponent");
    let exp: i32 = exp.parse().expect("exponent is an integer");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if digits.chars().all(|c| c == '0') {
        return format!("{sign}0");
    }
    // The decimal point's position counted from the left of the digits, as .NET's
    // number.Scale: 1.5 has scale 1, 0.015 has scale -1.
    let scale = exp + 1;
    if scale > max_scale || scale < -3 {
        let (first, rest) = digits.split_at(1);
        let exp_sign = if exp < 0 { '-' } else { '+' };
        let body = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        return format!("{sign}{body}E{exp_sign}{:02}", exp.abs());
    }
    let n = digits.len() as i32;
    if scale <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-scale) as usize))
    } else if scale >= n {
        format!("{sign}{digits}{}", "0".repeat((scale - n) as usize))
    } else {
        let (int, frac) = digits.split_at(scale as usize);
        format!("{sign}{int}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_escaping_matches_javascript_encoder_default() {
        // Captured from .NET 9: JsonSerializer.Serialize(s).
        let s = "a\"b\\c/d<e>f&g'h+i`j\n\r\t\u{8}\u{c}\u{1}\u{7f} \u{e9} \u{25b8} \u{1f6e0} \u{2028} ~";
        let want = concat!(
            r#""a\u0022b\\c/d\u003Ce\u003Ef\u0026g\u0027h\u002Bi\u0060j\n\r\t\b\f"#,
            r#"\u0001\u007F \u00E9 \u25B8 \uD83D\uDEE0 \u2028 ~""#
        );
        assert_eq!(to_string(s), want);
    }

    #[test]
    fn relaxed_escaping_matches_unsafe_relaxed_json_escaping() {
        let s = "a\"b\\c/d<e>f&g'h+i`j\n\r\t\u{8}\u{c}\u{1}\u{7f} \u{e9} \u{25b8} \u{1f6e0} \u{2028} ~";
        let opts = Options {
            escaping: Escaping::Relaxed,
            indented: false,
        };
        let want = concat!(
            r#""a\"b\\c/d<e>f&g'h+i`j\n\r\t\b\f\u0001\u007F "#,
            "\u{e9} \u{25b8} ",
            r#"\uD83D\uDEE0 \u2028 ~""#
        );
        assert_eq!(to_string_with(s, opts), want);
    }

    #[test]
    fn doubles_match_dotnet_round_trip_format() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (1.0, "1"),
            (-1.0, "-1"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (0.30000000000000004, "0.30000000000000004"),
            (1e-5, "1E-05"),
            (1e-7, "1E-07"),
            (0.0001, "0.0001"),
            (1e14, "100000000000000"),
            (1e15, "1000000000000000"),
            (1e16, "10000000000000000"),
            (123456789012345678.0, "1.2345678901234568E+17"),
            (1.5e300, "1.5E+300"),
            (3.14159, "3.14159"),
            (-0.0, "-0"),
            (2.5e-10, "2.5E-10"),
            (100.0, "100"),
            (1e21, "1E+21"),
            (12345.678, "12345.678"),
        ];
        for (v, want) in cases {
            assert_eq!(format_double(*v), *want, "for {v:e}");
        }
        assert_eq!(format_single(1.1f32), "1.1");
        assert_eq!(format_single(3.4e38f32), "3.4E+38");
    }

    #[test]
    fn indented_output_matches_write_indented() {
        let v = json!({"A": [], "B": {}, "C": null, "D": [1, 2]});
        assert_eq!(
            to_string_indented(&v),
            "{\n  \"A\": [],\n  \"B\": {},\n  \"C\": null,\n  \"D\": [\n    1,\n    2\n  ]\n}"
        );
    }

    #[test]
    fn compact_output_has_no_spaces() {
        let v = json!({"a": [1, {"b": "\u{e9}"}]});
        assert_eq!(to_string(&v), r#"{"a":[1,{"b":"\u00E9"}]}"#);
    }
}
