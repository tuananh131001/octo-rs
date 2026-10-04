//! Last.fm with I/O (`Services/LastFm`): the web-service client the radio and search read, and
//! the scrobbler with the dashboard's Connect flow. The records, the search cleanup and the
//! request signature are `octo_core::last_fm`.

pub mod last_fm_scrobble_service;
pub mod last_fm_service;

pub use last_fm_scrobble_service::{LastFmConnectError, LastFmScrobbleService, ScrobbleTime, ScrobbleTuning};
pub use last_fm_service::LastFmService;

/// The .NET number parsers the Last.fm readers lean on.
pub(crate) mod net_parse {
    /// The white space `NumberStyles.AllowLeadingWhite`/`AllowTrailingWhite` skip.
    fn trim(text: &str) -> &str {
        text.trim_matches(|c: char| c == ' ' || ('\u{9}'..='\u{d}').contains(&c))
    }

    /// `long.TryParse(text)` (`NumberStyles.Integer`): optional white space, an optional sign,
    /// and decimal digits that fit.
    pub fn parse_long(text: &str) -> Option<i64> {
        let text = trim(text);
        let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    }

    /// `double.TryParse(text, style, InvariantCulture)` with `NumberStyles.Float`, plus
    /// `AllowThousands` (commas in the whole part) when `thousands`. "Infinity" and "NaN" read
    /// as .NET reads them; Rust's own "inf" does not.
    pub fn parse_double(text: &str, thousands: bool) -> Option<f64> {
        let text = trim(text);
        let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
        let lower = unsigned.to_ascii_lowercase();
        if lower == "infinity" || lower == "nan" {
            return text.parse().ok();
        }
        if unsigned.is_empty() || !unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
            return None;
        }
        let whole_end = unsigned.find(['.', 'e', 'E']).unwrap_or(unsigned.len());
        let candidate = if thousands && unsigned[..whole_end].contains(',') {
            if unsigned.starts_with(',') {
                return None;
            }
            text.replacen(&unsigned[..whole_end], &unsigned[..whole_end].replace(',', ""), 1)
        } else {
            text.to_string()
        };
        if !candidate
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            return None;
        }
        candidate.parse().ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn numbers_read_as_dotnet_reads_them() {
            assert_eq!(parse_long(" 180000 "), Some(180_000));
            assert_eq!(parse_long("-5"), Some(-5));
            assert_eq!(parse_long("+5"), Some(5));
            assert_eq!(parse_long("1.5"), None);
            assert_eq!(parse_long("1e3"), None);
            assert_eq!(parse_long(""), None);
            assert_eq!(parse_long("True"), None);
            assert_eq!(parse_long("99999999999999999999"), None);
            assert_eq!(parse_double("0.9", false), Some(0.9));
            assert_eq!(parse_double(" -1e3 ", false), Some(-1000.0));
            assert_eq!(parse_double(".5", false), Some(0.5));
            assert_eq!(parse_double("1,000.5", true), Some(1000.5));
            assert_eq!(parse_double("1,000.5", false), None);
            assert_eq!(parse_double("inf", false), None);
            assert_eq!(parse_double("Infinity", false), Some(f64::INFINITY));
            assert!(parse_double("NaN", false).is_some_and(f64::is_nan));
            assert_eq!(parse_double("True", false), None);
            assert_eq!(parse_double("", false), None);
        }
    }
}
