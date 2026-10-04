//! The pure half of `Services/Lidarr/LidarrClient.cs`: its records, the readers for Lidarr's
//! JSON (with `JsonNode.GetValue<T>`'s strictness), and the album choice. The HTTP side is
//! `octo::services::lidarr::lidarr_client`.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::common::SongIdentity;
use crate::common::dotnet;

/// What escaped the C# Lidarr classes, by exception type, so callers can branch on the kind as
/// the C# `catch` clauses did. Each carries the exception's message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LidarrError {
    /// `InvalidOperationException`: not set up, no single album, an answer of the wrong shape,
    /// and `JsonNode.GetValue<T>` on a value of the wrong kind.
    #[error("{0}")]
    InvalidOperation(String),
    /// `HttpRequestException`: Lidarr answered with an error status, or could not be reached.
    #[error("{0}")]
    Http(String),
    /// `TaskCanceledException` / `OperationCanceledException`: the client's timeout, or the
    /// caller's token.
    #[error("{0}")]
    Canceled(String),
    /// `JsonException`: an answer that is not JSON.
    #[error("{0}")]
    Json(String),
    /// `ArgumentException`: two track files with one id (`ToDictionary`).
    #[error("{0}")]
    Argument(String),
    /// `FileNotFoundException`: the track fetcher found no album, or no good enough copy.
    #[error("{0}")]
    FileNotFound(String),
    /// `IOException`: copying the file out failed.
    #[error("{0}")]
    Io(String),
}

impl LidarrError {
    /// `ct.ThrowIfCancellationRequested()`'s `OperationCanceledException`.
    pub fn operation_canceled() -> Self {
        LidarrError::Canceled("The operation was canceled.".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LidarrChoice {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LidarrRootFolder {
    pub id: i32,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LidarrOptions {
    pub root_folders: Vec<LidarrRootFolder>,
    pub quality_profiles: Vec<LidarrChoice>,
    pub metadata_profiles: Vec<LidarrChoice>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LidarrAlbumCandidate {
    pub id: i32,
    pub foreign_album_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    /// The lookup row as Lidarr sent it, which is what adding the album posts back.
    pub resource: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrImportedTrack {
    pub id: i32,
    pub title: String,
    pub track_number: Option<i32>,
    pub duration_seconds: Option<i32>,
    pub has_file: bool,
    pub path: Option<String>,
    pub size_bytes: i64,
    pub artist: Option<String>,
    /// 0 when the track has no file (the C# default).
    pub track_file_id: i32,
    pub quality: Option<String>,
}

/// An album Lidarr already has, as it stood before Octo touched it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LidarrAlbumState {
    pub id: i32,
    pub monitored: bool,
}

/// A search Lidarr accepted, and whether Octo added or monitored the album to get it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LidarrSearchStarted {
    pub album_id: i32,
    pub existed: bool,
    pub was_monitored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrAlbumImportState {
    pub tracks: Vec<LidarrImportedTrack>,
    pub track_count: i32,
    pub track_file_count: i32,
}

impl LidarrAlbumImportState {
    pub fn is_complete(&self) -> bool {
        self.track_count > 0 && self.track_file_count >= self.track_count
    }
}

// --- Reading Lidarr's JSON the way System.Text.Json's JsonNode did. A missing property and a
// JSON null are both C#'s null; a value of another kind makes `GetValue<T>` throw. ---

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

fn cannot_convert(value: &Value, type_name: &str) -> LidarrError {
    match value {
        Value::Array(_) | Value::Object(_) => {
            LidarrError::InvalidOperation("The node must be of type 'JsonValue'.".to_string())
        }
        _ => LidarrError::InvalidOperation(format!(
            "An element of type '{}' cannot be converted to a '{type_name}'.",
            kind_name(value)
        )),
    }
}

/// `o[key]`: the property, or None when it is missing or null.
pub fn property<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

/// `node?[key]` on a node that may not be an object: the indexer threw for anything else.
pub fn child<'a>(node: &'a Value, key: &str) -> Result<Option<&'a Value>, LidarrError> {
    match node {
        Value::Object(o) => Ok(property(o, key)),
        _ => Err(LidarrError::InvalidOperation(
            "The node must be of type 'JsonObject'.".to_string(),
        )),
    }
}

/// `o[key] as JsonObject`.
pub fn object<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    o.get(key).and_then(Value::as_object)
}

/// `value.GetValue<string>()`.
pub fn value_str(value: &Value) -> Result<&str, LidarrError> {
    value
        .as_str()
        .ok_or_else(|| cannot_convert(value, "System.String"))
}

/// `NullableStr`: `o[key]?.GetValue<string>()`.
pub fn nullable_str(o: &Map<String, Value>, key: &str) -> Result<Option<String>, LidarrError> {
    property(o, key)
        .map(|v| value_str(v).map(str::to_string))
        .transpose()
}

/// `Str`: `NullableStr(o, key) ?? ""`.
pub fn str_or_empty(o: &Map<String, Value>, key: &str) -> Result<String, LidarrError> {
    Ok(nullable_str(o, key)?.unwrap_or_default())
}

/// `NullableInt`: `o[key]?.GetValue<int>()`. A number with a fraction or out of range throws,
/// as `JsonElement.TryGetInt32` refused it.
pub fn nullable_int(o: &Map<String, Value>, key: &str) -> Result<Option<i32>, LidarrError> {
    property(o, key)
        .map(|v| {
            v.as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| cannot_convert(v, "System.Int32"))
        })
        .transpose()
}

/// `Int`: `NullableInt(o, key) ?? 0`.
pub fn int_or_zero(o: &Map<String, Value>, key: &str) -> Result<i32, LidarrError> {
    Ok(nullable_int(o, key)?.unwrap_or(0))
}

/// `NullableLong`: `o[key]?.GetValue<long>()`.
pub fn nullable_long(o: &Map<String, Value>, key: &str) -> Result<Option<i64>, LidarrError> {
    property(o, key)
        .map(|v| v.as_i64().ok_or_else(|| cannot_convert(v, "System.Int64")))
        .transpose()
}

/// `o[key]?.GetValue<bool>()`.
pub fn nullable_bool(o: &Map<String, Value>, key: &str) -> Result<Option<bool>, LidarrError> {
    property(o, key)
        .map(|v| v.as_bool().ok_or_else(|| cannot_convert(v, "System.Boolean")))
        .transpose()
}

/// `MapOptions`: the server's root folders and profiles.
pub fn map_options(
    roots: &[Map<String, Value>],
    quality: &[Map<String, Value>],
    metadata: &[Map<String, Value>],
) -> Result<LidarrOptions, LidarrError> {
    let choice = |x: &Map<String, Value>| -> Result<LidarrChoice, LidarrError> {
        Ok(LidarrChoice {
            id: int_or_zero(x, "id")?,
            name: str_or_empty(x, "name")?,
        })
    };
    Ok(LidarrOptions {
        root_folders: roots
            .iter()
            .map(|x| {
                Ok(LidarrRootFolder {
                    id: int_or_zero(x, "id")?,
                    path: str_or_empty(x, "path")?,
                })
            })
            .collect::<Result<_, LidarrError>>()?,
        quality_profiles: quality.iter().map(choice).collect::<Result<_, _>>()?,
        metadata_profiles: metadata.iter().map(choice).collect::<Result<_, _>>()?,
    })
}

/// `ParseAlbum`: one row of an album lookup, its year from the first four characters of its
/// release date.
pub fn parse_album(row: &Map<String, Value>) -> Result<LidarrAlbumCandidate, LidarrError> {
    let artist = object(row, "artist");
    let date = nullable_str(row, "releaseDate")?;
    let year = date.as_deref().and_then(|d| {
        // date[..4] counts UTF-16 units; a cut surrogate pair is no number either.
        if dotnet::utf16_len(d) < 4 {
            return None;
        }
        let units: Vec<u16> = d.encode_utf16().take(4).collect();
        String::from_utf16(&units)
            .ok()
            .and_then(|head| int_try_parse(&head))
    });
    Ok(LidarrAlbumCandidate {
        id: nullable_int(row, "id")?.unwrap_or(0),
        foreign_album_id: nullable_str(row, "foreignAlbumId")?.unwrap_or_default(),
        title: nullable_str(row, "title")?.unwrap_or_default(),
        artist: match artist {
            Some(a) => nullable_str(a, "artistName")?.unwrap_or_default(),
            None => String::new(),
        },
        year,
        resource: row.clone(),
    })
}

/// The album whose artist and title read the same as asked, by [`SongIdentity::key`]; several
/// releases of it are told apart by the year. None when there is no single one.
pub fn select_best_album<'a>(
    candidates: &'a [LidarrAlbumCandidate],
    artist: &str,
    album: &str,
    year: Option<i32>,
) -> Option<&'a LidarrAlbumCandidate> {
    let wanted_artist = SongIdentity::key(artist);
    let wanted_album = SongIdentity::key(album);
    let mut exact: Vec<&LidarrAlbumCandidate> = Vec::new();
    for c in candidates {
        if SongIdentity::key(&c.artist) == wanted_artist
            && SongIdentity::key(&c.title) == wanted_album
            && !exact
                .iter()
                .any(|e| dotnet::eq_ignore_case(&e.foreign_album_id, &c.foreign_album_id))
        {
            exact.push(c);
        }
    }
    if exact.len() == 1 {
        return Some(exact[0]);
    }
    if exact.is_empty() {
        return None;
    }

    if let Some(y) = year {
        let same_year: Vec<_> = exact.iter().filter(|c| c.year == Some(y)).collect();
        if same_year.len() == 1 {
            return Some(same_year[0]);
        }
    }

    // Multiple releases with the same display identity are unsafe to choose
    // automatically. A wrong album is much worse than an actionable failure.
    None
}

/// `ParseTrackNumber`: "3", "1-3", "A/2" and the like, by the part before the first `-`, `/`
/// or `.`.
pub fn parse_track_number(value: &str) -> Option<i32> {
    let head = value.split(['-', '/', '.']).next().unwrap_or("");
    int_try_parse(head)
}

/// `int.TryParse(s)`: `NumberStyles.Integer`, so white space around it and a leading sign.
fn int_try_parse(s: &str) -> Option<i32> {
    let is_white = |c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ');
    let s = s.trim_matches(is_white);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits.bytes() {
        n = n.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
        if n > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    i32::try_from(if negative { -n } else { n }).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn candidate(
        foreign_id: &str,
        title: &str,
        artist: &str,
        year: Option<i32>,
    ) -> LidarrAlbumCandidate {
        let resource = json!({
            "foreignAlbumId": foreign_id,
            "title": title,
            "releaseDate": year.map(|y| format!("{y}-01-01T00:00:00Z")),
            "artist": { "artistName": artist, "foreignArtistId": "artist-mbid" },
        });
        LidarrAlbumCandidate {
            id: 0,
            foreign_album_id: foreign_id.into(),
            title: title.into(),
            artist: artist.into(),
            year,
            resource: resource.as_object().cloned().unwrap_or_default(),
        }
    }

    // LidarrClientTests.AlbumSelectionRequiresExactIdentityAndUsesYearToDisambiguate
    #[test]
    fn album_selection_requires_exact_identity_and_uses_year_to_disambiguate() {
        let candidates = [
            candidate("a", "In Rainbows", "Radiohead", Some(2007)),
            candidate("b", "In Rainbows", "Radiohead", Some(2025)),
        ];

        assert_eq!(
            select_best_album(&candidates, "RADIOHEAD", "In-Rainbows", Some(2007))
                .map(|c| c.foreign_album_id.as_str()),
            Some("a")
        );
        assert!(select_best_album(&candidates, "Radiohead", "In Rainbows", None).is_none());
        assert!(select_best_album(&candidates, "Other", "In Rainbows", Some(2007)).is_none());
    }

    /// Two rows of one release group are one album, compared ignoring case.
    #[test]
    fn one_release_group_listed_twice_is_one_album() {
        let candidates = [
            candidate("RG", "In Rainbows", "Radiohead", Some(2007)),
            candidate("rg", "In Rainbows", "Radiohead", Some(2008)),
        ];
        assert_eq!(
            select_best_album(&candidates, "Radiohead", "In Rainbows", None).map(|c| c.year),
            Some(Some(2007))
        );
    }

    #[test]
    fn track_numbers_and_years_parse_as_int_try_parse_did() {
        for (text, expected) in [
            ("3", Some(3)),
            ("1-3", Some(1)),
            ("2/12", Some(2)),
            (" 4 ", Some(4)),
            ("+5", Some(5)),
            ("A1", None),
            ("", None),
            ("-1", None),
            ("2147483648", None),
        ] {
            assert_eq!(parse_track_number(text), expected, "{text:?}");
        }
        for (date, year) in [
            (Some("1998-04-20"), Some(1998)),
            (Some("199"), None),
            (Some(" 199"), Some(199)),
            (Some("19é8"), None),
            (None, None),
        ] {
            let row = json!({ "releaseDate": date, "foreignAlbumId": "x" });
            let parsed = parse_album(row.as_object().expect("an object")).expect("a row");
            assert_eq!(parsed.year, year, "{date:?}");
        }
    }

    /// `GetValue<T>` threw on a value of another kind, rather than reading it as missing.
    #[test]
    fn a_value_of_the_wrong_kind_throws_as_get_value_did() {
        let row = json!({ "id": "7", "title": 5, "size": 1.5, "ok": null });
        let row = row.as_object().expect("an object");
        assert_eq!(
            nullable_int(row, "id"),
            Err(LidarrError::InvalidOperation(
                "An element of type 'String' cannot be converted to a 'System.Int32'.".into()
            ))
        );
        assert!(nullable_str(row, "title").is_err());
        assert!(nullable_long(row, "size").is_err());
        assert_eq!(nullable_int(row, "ok"), Ok(None));
        assert_eq!(int_or_zero(row, "missing"), Ok(0));
    }

    // LidarrClientTests.ImportCompletionUsesAlbumStatisticsNotAlternateReleaseRows (the
    // IsComplete half; the HTTP half is in octo's client tests)
    #[test]
    fn an_album_is_complete_once_every_track_has_a_file() {
        let state = |count, files| LidarrAlbumImportState {
            tracks: Vec::new(),
            track_count: count,
            track_file_count: files,
        };
        assert!(state(1, 1).is_complete());
        assert!(!state(2, 1).is_complete());
        assert!(!state(0, 0).is_complete());
    }
}
