//! Port of `Services/Metadata/AcceptLanguageHeader.cs`.
//!
//! The C# handed each segment to `HttpHeaders.AcceptLanguage.TryParseAdd`, which kept it only
//! when .NET's `StringWithQualityHeaderValue` parser accepted it, and sent the kept values
//! joined with ", " (`en-US, en; q=0.9`). That parser is ported here, so the same values are
//! kept and the header reads the same.

use std::fmt;

use http::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderValue};

use crate::common::dotnet::{format_optional_decimals, is_blank};
use crate::settings::MetadataSettings;

/// One Accept-Language entry as .NET parsed it: a token and an optional quality.
#[derive(Debug, Clone, PartialEq)]
pub struct StringWithQuality {
    pub value: String,
    pub quality: Option<f64>,
}

impl fmt::Display for StringWithQuality {
    /// `StringWithQualityHeaderValue.ToString()`: the quality written with `"0.0##"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.quality {
            None => f.write_str(&self.value),
            Some(quality) => {
                let mut text = format_optional_decimals(quality, 3);
                if !text.contains('.') {
                    text.push_str(".0");
                }
                write!(f, "{}; q={text}", self.value)
            }
        }
    }
}

/// Applies the configured metadata language as an Accept-Language header.
/// An invalid configured value must never break client construction, so a
/// value the header parser rejects is simply not sent.
pub struct AcceptLanguageHeader;

impl AcceptLanguageHeader {
    /// The values `Apply` added to the client's default headers, in order.
    pub fn values(settings: &MetadataSettings) -> Vec<StringWithQuality> {
        let configured = settings.language.as_str();
        if is_blank(configured) {
            return Vec::new();
        }
        // Tolerate a pasted browser-style list ("en-US,en;q=0.9"): add each
        // segment on its own so one bad segment cannot take out the rest.
        configured
            .split(',')
            .map(str::trim)
            .filter(|lang| !lang.is_empty())
            .filter_map(parse)
            .collect()
    }

    /// The header as it goes out, or None when nothing is sent.
    pub fn header_value(settings: &MetadataSettings) -> Option<String> {
        let values = Self::values(settings);
        if values.is_empty() {
            return None;
        }
        Some(
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    /// Replaces the Accept-Language default header (for `reqwest::ClientBuilder::default_headers`):
    /// cleared first, so applying again replaces instead of stacking.
    pub fn apply(headers: &mut HeaderMap, settings: &MetadataSettings) {
        headers.remove(ACCEPT_LANGUAGE);
        if let Some(value) = Self::header_value(settings).and_then(|value| HeaderValue::from_str(&value).ok())
        {
            headers.insert(ACCEPT_LANGUAGE, value);
        }
    }
}

/// `HttpRuleParser.IsTokenChar`: visible ASCII except the separators.
fn is_token_char(c: char) -> bool {
    c.is_ascii_graphic() && !"()<>@,;:\\\"/[]?={}".contains(c)
}

/// `HttpRuleParser.GetWhitespaceLength`: spaces, tabs, and a CRLF folded before one of them.
fn whitespace_length(input: &[char], start: usize) -> usize {
    let mut current = start;
    while current < input.len() {
        match input[current] {
            ' ' | '\t' => current += 1,
            '\r' if current + 2 < input.len()
                && input[current + 1] == '\n'
                && matches!(input[current + 2], ' ' | '\t') =>
            {
                current += 3
            }
            _ => break,
        }
    }
    current - start
}

/// `HttpRuleParser.GetNumberLength` with decimals allowed: digits and at most one dot, never
/// starting with the dot.
fn number_length(input: &[char], start: usize) -> usize {
    if input.get(start) == Some(&'.') {
        return 0;
    }
    let mut current = start;
    let mut have_dot = false;
    while let Some(&c) = input.get(current) {
        if c.is_ascii_digit() {
            current += 1;
        } else if !have_dot && c == '.' {
            have_dot = true;
            current += 1;
        } else {
            break;
        }
    }
    current - start
}

/// `TryParseAdd` of one trimmed segment, without commas: the whole of it must be one
/// `token [; q=number]` with the quality between 0 and 1.
fn parse(segment: &str) -> Option<StringWithQuality> {
    let input: Vec<char> = segment.chars().collect();
    let token_length = input.iter().take_while(|c| is_token_char(**c)).count();
    if token_length == 0 {
        return None;
    }
    let value: String = input[..token_length].iter().collect();
    let mut current = token_length;
    current += whitespace_length(&input, current);
    if current == input.len() {
        return Some(StringWithQuality { value, quality: None });
    }
    if input[current] != ';' {
        // Something follows the value that is neither a quality nor a separator.
        return None;
    }
    current += 1;
    current += whitespace_length(&input, current);
    if !matches!(input.get(current), Some('q' | 'Q')) {
        return None;
    }
    current += 1;
    current += whitespace_length(&input, current);
    if input.get(current) != Some(&'=') {
        return None;
    }
    current += 1;
    current += whitespace_length(&input, current);
    if current == input.len() {
        return None;
    }
    let length = number_length(&input, current);
    if length == 0 {
        return None;
    }
    let number: String = input[current..current + length].iter().collect();
    let quality: f64 = number.parse().ok()?;
    if !(0.0..=1.0).contains(&quality) {
        return None;
    }
    current += length;
    current += whitespace_length(&input, current);
    (current == input.len()).then_some(StringWithQuality {
        value,
        quality: Some(quality),
    })
}

/// [`AcceptLanguageHeader::header_value`] as a free function, for the clients that only need the
/// header's text.
pub fn header_value(settings: &MetadataSettings) -> Option<String> {
    AcceptLanguageHeader::header_value(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(language: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        AcceptLanguageHeader::apply(
            &mut headers,
            &MetadataSettings {
                language: language.to_string(),
                ..Default::default()
            },
        );
        headers
    }

    fn values(language: &str) -> Vec<String> {
        AcceptLanguageHeader::values(&MetadataSettings {
            language: language.to_string(),
            ..Default::default()
        })
        .into_iter()
        .map(|value| value.value)
        .collect()
    }

    fn header(headers: &HeaderMap) -> Option<&str> {
        headers.get(ACCEPT_LANGUAGE).and_then(|value| value.to_str().ok())
    }

    // ---- AcceptLanguageHeaderTests.cs ----------------------------------------------------
    // The header this applies is what keeps Deezer from localizing genre names to the
    // server's IP country (issue #24), so the interesting cases are the ones where a
    // user-typed value is messy: blank, padded, garbage, or a whole browser-style list.

    #[test]
    fn sets_the_header_for_a_simple_code() {
        assert_eq!(values("en"), ["en"]);
        assert_eq!(header(&applied("en")), Some("en"));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(values("  de  "), ["de"]);
    }

    #[test]
    fn empty_value_leaves_the_header_unset() {
        for language in ["", "   "] {
            assert!(applied(language).get(ACCEPT_LANGUAGE).is_none(), "{language:?}");
        }
    }

    /// A value the header parser rejects must degrade to provider-default behavior, never
    /// break client construction.
    #[test]
    fn garbage_value_leaves_the_header_unset() {
        assert!(applied("???").get(ACCEPT_LANGUAGE).is_none());
    }

    #[test]
    fn accepts_a_browser_style_list() {
        assert_eq!(values("en-US,en;q=0.9"), ["en-US", "en"]);
        assert_eq!(header(&applied("en-US,en;q=0.9")), Some("en-US, en; q=0.9"));
    }

    #[test]
    fn reapplying_replaces_instead_of_stacking() {
        let mut headers = HeaderMap::new();
        let settings = |language: &str| MetadataSettings {
            language: language.to_string(),
            ..Default::default()
        };
        AcceptLanguageHeader::apply(&mut headers, &settings("de"));
        AcceptLanguageHeader::apply(&mut headers, &settings("en"));

        let all: Vec<_> = headers.get_all(ACCEPT_LANGUAGE).iter().collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], "en");
    }

    // ---- Rust-only: what .NET 9's parser answered for these ------------------------------

    #[test]
    fn the_header_is_what_dotnet_sent() {
        for (language, expected) in [
            ("en;q=1", Some("en; q=1.0")),
            ("en; q = 0.8765", Some("en; q=0.877")),
            ("en;q=2", None),
            ("en;q=.5", None),
            ("en;q=1.", Some("en; q=1.0")),
            ("en q", None),
            ("*", Some("*")),
            ("en_US", Some("en_US")),
            ("fr;Q=0.5", Some("fr; q=0.5")),
            ("日本", None),
            ("en;q=0.5;x=1", None),
            ("en,,de", Some("en, de")),
            (" , ", None),
            ("en ;q=0.25", Some("en; q=0.25")),
            ("en;q=0", Some("en; q=0.0")),
            ("en;q=0.0005", Some("en; q=0.001")),
        ] {
            let settings = MetadataSettings {
                language: language.to_string(),
                ..Default::default()
            };
            assert_eq!(
                AcceptLanguageHeader::header_value(&settings).as_deref(),
                expected,
                "{language:?}"
            );
        }
    }
}
