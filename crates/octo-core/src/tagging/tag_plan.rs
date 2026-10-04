//! Port of the report types of `Services/Tagging/TagPlan.cs`, which `downloads-history.json`
//! carries, and a stub of `TagPlan` itself.
//!
//! STUB(wave 2 tagging): `TagPlan` is replaced when the tagging port lands. `TagReport`,
//! `TagReportCandidate` and `FieldDecision` are complete ports (they are written into
//! `downloads-history.json` and checked against its fixture); the tagging port keeps them.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// One field's value and where it came from, for the report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FieldDecision {
    pub value: Option<String>,
    pub source: Option<String>,
}

/// What the chooser decided for one download: the release that won, how sure it is, every
/// candidate it weighed, and what each field is set to and from. Applied to the Song before the
/// file is placed; kept as a report in the fetched-songs log.
// STUB(wave 2 tagging): replaced when the tagging port lands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagPlan {}

/// What goes into the fetched-songs log and the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TagReport {
    /// The `TagConfidence` name.
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub confidence: String,
    pub distance: Option<f64>,
    pub release_title: Option<String>,
    pub release_id: Option<String>,
    /// The `TagSource` name.
    pub source: Option<String>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub candidates: Vec<TagReportCandidate>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub fields: IndexMap<String, FieldDecision>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub notes: Vec<String>,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub stage_seconds: IndexMap<String, f64>,
    pub rehearsed: bool,
    pub details_prefetch_hit: bool,
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbfs: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TagReportCandidate {
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub source: String,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub title: String,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub album: String,
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    pub date: Option<String>,
    pub distance: f64,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub biggest_penalties: Vec<String>,
}
