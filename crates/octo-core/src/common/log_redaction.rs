//! Port of the pure half of `Services/Common/LogRedaction.cs`: the redaction itself. Wrapping
//! the log pipeline (the C# `RedactingLoggerFactory`, `RedactingLogger` and
//! `RedactedLogState`) belongs to the tracing setup in the `octo` crate.
//!
//! Keeps Subsonic credentials out of the log. Subsonic clients sign in through the query string,
//! so every request line written ("Request starting ... /rest/star?u=...&t=...&s=...")
//! carries a token and salt that replay as that user, and `p=` and `apiKey=` are worse.
//!
//! The redaction wraps the logging pipeline rather than any one log statement, so it holds for
//! request lines, HTTP client lines and Octo's own, at whatever level an operator turns them
//! up to. Only the values are masked; the lines keep their shape.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

pub const MASK: &str = "***";

// t and s are the token login, p the password (plain or enc:), apiKey the OpenSubsonic key,
// token what several clients call theirs, api_key Last.fm's key and client AcoustID's.
// Subsonic reads names case-insensitively, so this does. sk is a Last.fm session key,
// which scrobbles as that listener, and api_sig is signed with the Last.fm shared secret.
// user is the AcoustID user's own API key, sent with every submission. Subsonic's u is
// not it, and stays readable.
//
// The C# matched the value behind a lookbehind, `(?<=[?&;](?:...)=)[^&#\s"'<>]+`. Here the
// name is group 1 and is written back. The two find the same values: a value never holds an
// `&`, so the next parameter's `[?&;]name=` always starts at or after the end of the last value.
//
// Every `s` is spelled `(?-i:[sS])`: Rust's case folding also matches the long s (ſ) there,
// and .NET's invariant IgnoreCase does not.
static SECRET_PARAMETER: LazyLock<Regex> = LazyLock::new(|| {
    let s = "(?-i:[sS])";
    Regex::new(&format!(
        r#"(?i)([?&;](?:t|{s}|p|apikey|token|api_key|client|u{s}er|{s}k|api_{s}ig)=)[^&#\s"'<>]+"#
    ))
    .expect("a fixed pattern compiles")
});

/// The text with every secret query parameter's value replaced by [`MASK`].
/// Borrows the text unchanged when there is nothing to mask.
pub fn redact(text: &str) -> Cow<'_, str> {
    if !text.contains('=') {
        return Cow::Borrowed(text);
    }
    SECRET_PARAMETER.replace_all(text, |caps: &Captures| format!("{}{MASK}", &caps[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_NAMES: [&str; 16] = [
        "t", "s", "p", "apiKey", "token", "api_key", "client", "sk", "api_sig", "T", "APIKEY", "Token",
        "API_KEY", "Client", "user", "User",
    ];

    #[test]
    fn redact_masks_only_secret_values() {
        let cases = [
            ("/rest/ping?u=a&t=abc&s=def", "/rest/ping?u=a&t=***&s=***"),
            ("?p=enc:6162&u=a", "?p=***&u=a"),
            ("?apikey=K1&Token=K2#top", "?apikey=***&Token=***#top"),
            (
                "/2.0/?method=track.search&api_key=K1&format=json",
                "/2.0/?method=track.search&api_key=***&format=json",
            ),
            (
                "v2/lookup?client=K1&meta=recordings",
                "v2/lookup?client=***&meta=recordings",
            ),
            ("GET /rest/x?t=abc - 200", "GET /rest/x?t=*** - 200"),
            (
                "<a href='/rest/x?u=a&amp;t=abc'>",
                "<a href='/rest/x?u=a&amp;t=***'>",
            ),
            // Names that only start or end like a secret, and empty values, are left alone.
            (
                "?ts=1&st=2&sort=3&apiKeyId=4&tokens=5&clientId=6&api_keys=7&c=Octo",
                "?ts=1&st=2&sort=3&apiKeyId=4&tokens=5&clientId=6&api_keys=7&c=Octo",
            ),
            ("?t=&s=", "?t=&s="),
            ("no query here, t=abc", "no query here, t=abc"),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    #[test]
    fn case_is_ignored_as_dotnet_ignored_it() {
        // Checked against .NET 9's invariant IgnoreCase: the Kelvin sign is a k, the long s
        // is not an s.
        assert_eq!(
            redact("?apiKEY=1&S=2&\u{212A}=3&s\u{212A}=4"),
            "?apiKEY=***&S=***&\u{212A}=3&s\u{212A}=***"
        );
        assert_eq!(
            redact("?\u{17F}=secret&u\u{17F}er=x"),
            "?\u{17F}=secret&u\u{17F}er=x"
        );
    }

    #[test]
    fn redact_returns_the_same_string_when_nothing_is_masked() {
        let text = "/rest/ping?u=winters&v=1.16.1";
        assert!(matches!(redact(text), Cow::Borrowed(same) if std::ptr::eq(same, text)));
    }

    #[test]
    fn request_lines_mask_each_secret_parameter() {
        // The pure core of RequestLines_MaskEachSecretParameter and OctosOwnLines_*: each
        // secret name, sent on its own, is masked and the rest of the line kept.
        for name in SECRET_NAMES {
            let secret = format!("Secret{}", uuid::Uuid::new_v4().simple());
            let line = format!("/rest/ping.view?u=winters&{name}={secret}&v=1.16.1&c=Octo&f=json");
            let redacted = redact(&line);
            assert!(
                redacted.contains(&format!(
                    "/rest/ping.view?u=winters&{name}=***&v=1.16.1&c=Octo&f=json"
                )),
                "{name}: {redacted}"
            );
            assert!(!redacted.contains(&secret), "{name}");

            let own =
                format!("relay to http://navidrome:4533/rest/getSong?u=winters&{name}={secret}&id=7 failed");
            assert!(
                redact(&own).contains(&format!("u=winters&{name}=***&id=7")),
                "{name}"
            );
        }
    }

    #[test]
    fn request_lines_mask_every_secret_at_once_and_keep_the_rest() {
        let secret = || format!("Secret{}", uuid::Uuid::new_v4().simple());
        let (t, s, p, api_key, token) = (
            secret(),
            secret(),
            format!("enc:{}", secret()),
            secret(),
            secret(),
        );
        let line = format!(
            "Request finished HTTP/1.1 GET http://localhost/rest/star.view?u=winters&t={t}&s={s}&p={p}&apiKey={api_key}&token={token}&v=1.16.1&c=Octo&f=json&id=42 - 200"
        );

        let redacted = redact(&line);
        assert!(
            redacted.contains(
                "/rest/star.view?u=winters&t=***&s=***&p=***&apiKey=***&token=***&v=1.16.1&c=Octo&f=json&id=42"
            ),
            "{redacted}"
        );
        for secret in [t, s, p, api_key, token] {
            assert!(!redacted.contains(&secret));
        }
    }
}
