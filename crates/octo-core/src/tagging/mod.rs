//! Tagging (`Services/Tagging`): the evidence a download is weighed on, the candidates each
//! source offers, the distance and the chooser, the plan and its report, the album walk's
//! context, and the identifier and preview that drive them through traits the `octo` crate's
//! services implement.

pub mod album_tag_context;
pub mod candidate_sources;
pub mod matching_settings;
mod net;
pub mod release_chooser;
pub mod release_details;
pub mod release_distance;
pub mod release_identifier;
pub mod tag_evidence;
pub mod tag_plan;
pub mod tag_preview;

pub use album_tag_context::{AlbumTagContext, SettledRelease};
pub use candidate_sources::CandidateSources;
pub use matching_settings::MatchingSettings;
pub use release_chooser::ReleaseChooser;
pub use release_details::{ReleaseDetails, ReleaseGenre, ReleaseTrack};
pub use release_distance::{Distance, ReleaseDistance};
pub use release_identifier::{Cancelled, CatalogLookup, FileFactsReader, ReleaseIdentifier, ReleaseLookup};
pub use tag_evidence::{FileFacts, ReleaseCandidate, TagEvidence, TagRequest, TagSource};
pub use tag_plan::{
    BreakdownPart, FieldDecision, ScoredCandidate, TagConfidence, TagPlan, TagReport, TagReportCandidate,
};
pub use tag_preview::{
    CatalogBlankFiller, FingerprintVerifier, MeasuredLoudness, PreviewLoudnessMeter, ReplayGainText,
    TagPreview,
};
