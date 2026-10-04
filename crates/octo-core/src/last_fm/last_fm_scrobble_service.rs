//! The records and the request signature of `Services/LastFm/LastFmScrobbleService.cs`. The
//! queue, the Connect flow and the calls are `octo::services::last_fm::last_fm_scrobble_service`.

use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use serde::Serialize;

/// What Last.fm is told about one play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastFmTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
}

impl LastFmTrack {
    pub fn new(
        artist: impl Into<String>,
        title: impl Into<String>,
        album: Option<&str>,
        duration_seconds: Option<i32>,
    ) -> Self {
        Self {
            artist: artist.into(),
            title: title.into(),
            album: album.map(str::to_string),
            duration_seconds,
        }
    }
}

/// One Navidrome user as the dashboard's Last.fm scrobbling card shows them. Serialised as
/// ASP.NET wrote the record: camelCase, nulls written.
///
/// `approval_url`: the last.fm page that approves Octo, while a Connect is waiting on it.
/// `last_sent`: the latest play Last.fm took for this listener since Octo started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmScrobbleUser {
    pub user: String,
    pub connected: bool,
    pub last_fm_user: Option<String>,
    pub awaiting_approval: bool,
    pub notice: Option<String>,
    pub approval_url: Option<String>,
    pub last_sent: Option<LastFmSentPlay>,
}

/// A play Last.fm accepted, as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmSentPlay {
    pub artist: String,
    pub title: String,
    #[serde(with = "crate::json::datetime::utc")]
    pub played_at_utc: DateTime<Utc>,
}

/// What Last.fm said of an API key and shared secret, for the dashboard to show beside
/// each field. `key` is ok, invalid, missing or unreachable; `secret` is ok, invalid,
/// same-as-key, missing or unchecked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmCredentialCheck {
    pub key: String,
    pub secret: String,
    pub message: Option<String>,
}

impl LastFmCredentialCheck {
    pub fn new(key: &str, secret: &str, message: Option<String>) -> Self {
        Self {
            key: key.to_string(),
            secret: secret.to_string(),
            message,
        }
    }
}

/// A Last.fm refusal the dashboard can show as it is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LastFmScrobbleException {
    pub message: String,
    pub code: i32,
}

impl LastFmScrobbleException {
    pub fn new(message: impl Into<String>, code: i32) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }
}

/// Last.fm's request signature: every parameter except format and callback, sorted by name,
/// each name followed by its value, the shared secret on the end, then an MD5 of the UTF-8.
/// See last.fm/api/authspec, section 8.
pub fn sign<'a>(parameters: impl IntoIterator<Item = (&'a str, &'a str)>, secret: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = parameters
        .into_iter()
        .filter(|(name, _)| !matches!(*name, "format" | "callback" | "api_sig"))
        .collect();
    // StringComparer.Ordinal compares UTF-16 code units; the names are ASCII.
    pairs.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let mut text = String::new();
    for (name, value) in pairs {
        text.push_str(name);
        text.push_str(value);
    }
    text.push_str(secret);
    hex::encode(Md5::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_KEY: &str = "0123456789abcdef0123456789abcdef";
    const SECRET: &str = "s3cr3t";

    /// The authspec's own worked example, auth.getSession, with the MD5 worked out by hand
    /// outside this code (md5sum of "api_key..." + "method..." + "token..." + secret). format
    /// is sent but never signed.
    #[test]
    fn signature_matches_a_hand_computed_vector() {
        let parameters = [
            ("token", "tok123"),
            ("format", "json"),
            ("method", "auth.getSession"),
            ("api_key", API_KEY),
        ];

        assert_eq!(sign(parameters, SECRET), "5f50f7c80ec9a4fe05f95ce8ee49b1f8");
    }

    /// Sorted by name, spaces kept as they are, whatever order they were added in.
    #[test]
    fn signature_sorts_parameters_by_name() {
        let parameters = [
            ("track", "Be Nice 2 Me"),
            ("sk", "sk-alice"),
            ("method", "track.updateNowPlaying"),
            ("artist", "Bladee"),
            ("api_key", API_KEY),
            ("album", "Icedancer"),
        ];

        assert_eq!(sign(parameters, SECRET), "33dc1eb9105f636ce7cde482d248d31b");
    }

    /// Rust-only: the dashboard records as ASP.NET wrote them.
    #[test]
    fn the_dashboard_records_serialise_in_camel_case_with_nulls() {
        let user = LastFmScrobbleUser {
            user: "alice".into(),
            connected: true,
            last_fm_user: Some("lfm-alice".into()),
            awaiting_approval: false,
            notice: None,
            approval_url: None,
            last_sent: Some(LastFmSentPlay {
                artist: "Bladee".into(),
                title: "Be Nice 2 Me".into(),
                played_at_utc: DateTime::parse_from_rfc3339("2026-10-04T12:00:00.5Z")
                    .expect("a date")
                    .with_timezone(&Utc),
            }),
        };
        assert_eq!(
            crate::json::to_string(&user),
            r#"{"user":"alice","connected":true,"lastFmUser":"lfm-alice","awaitingApproval":false,"notice":null,"approvalUrl":null,"lastSent":{"artist":"Bladee","title":"Be Nice 2 Me","playedAtUtc":"2026-10-04T12:00:00.5Z"}}"#
        );
    }
}
