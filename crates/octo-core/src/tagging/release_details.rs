//! `Services/Tagging/ReleaseDetails.cs`.
//!
//! STUB(2-A tagging): replaced when 2-A lands. `MusicBrainzClient::lookup_release` returns one,
//! so the type and the "is this a release" test of `Parse` exist; the fields and the rest of
//! the reading come with the tagging port.

use serde_json::Value;

/// What one music database release lookup adds to a candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseDetails {
    pub release_id: String,
    pub title: Option<String>,
}

impl ReleaseDetails {
    /// Read the release lookup's answer. None for a document that is not a release.
    pub fn parse(root: &Value) -> Option<ReleaseDetails> {
        let object = root.as_object()?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())?;
        Some(ReleaseDetails {
            release_id: id.to_string(),
            title: object.get("title").and_then(Value::as_str).map(str::to_string),
        })
    }
}
