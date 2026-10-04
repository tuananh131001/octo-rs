//! Port of `Services/Tagging/ReleaseChooser.cs`.

use std::collections::HashMap;

use crate::common::song_identity::SongIdentity;
use crate::tagging::matching_settings::MatchingSettings;
use crate::tagging::net::{cmp_ordinal, ignore_case_key};
use crate::tagging::release_distance::ReleaseDistance;
use crate::tagging::tag_evidence::{ReleaseCandidate, TagEvidence};
use crate::tagging::tag_plan::{ScoredCandidate, TagConfidence, TagPlan};

/// Weighs every candidate release against the evidence, ranks them, and says how sure the
/// winner is. Strong may overwrite what the file and the catalog said; Medium may when the
/// fingerprint service or the music database backs it; anything less only fills blanks. Two
/// pressings of different albums too close to call leave the recording certain and the album
/// in doubt.
pub struct ReleaseChooser;

impl ReleaseChooser {
    pub const STRONG_THRESHOLD: f64 = 0.05;
    pub const MEDIUM_THRESHOLD: f64 = 0.25;
    pub const AMBIGUITY_MARGIN: f64 = 0.02;
    pub const MAX_CANDIDATES: usize = 200;

    pub fn choose(
        evidence: &TagEvidence,
        candidates: &[ReleaseCandidate],
        settings: &MatchingSettings,
        this_year: Option<i32>,
    ) -> TagPlan {
        let pool = &candidates[..candidates.len().min(Self::MAX_CANDIDATES)];
        if pool.is_empty() {
            return TagPlan::empty(Some(evidence.clone()), settings.clone());
        }

        // The earliest first release among each recording's candidates, so a later group of the
        // same recording reads as the reissue it is.
        let mut earliest: HashMap<String, i32> = HashMap::new();
        for c in pool {
            if let Some(year) = c.original_year() {
                let key = ignore_case_key(c.recording_id.as_deref().unwrap_or(""));
                earliest
                    .entry(key)
                    .and_modify(|min| *min = (*min).min(year))
                    .or_insert(year);
            }
        }

        let mut ranked: Vec<ScoredCandidate> = pool
            .iter()
            .map(|c| {
                let year = earliest
                    .get(&ignore_case_key(c.recording_id.as_deref().unwrap_or("")))
                    .copied();
                ReleaseDistance::measure(evidence, c, settings, year, this_year)
            })
            .collect();
        ranked.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| {
                    cmp_ordinal(
                        a.candidate.group_first_release_date.as_deref().unwrap_or("9999"),
                        b.candidate.group_first_release_date.as_deref().unwrap_or("9999"),
                    )
                })
                .then_with(|| {
                    cmp_ordinal(
                        a.candidate.release_date.as_deref().unwrap_or("9999"),
                        b.candidate.release_date.as_deref().unwrap_or("9999"),
                    )
                })
                .then_with(|| b.candidate.sources.cmp(&a.candidate.sources))
                .then_with(|| {
                    cmp_ordinal(
                        a.candidate.release_id.as_deref().unwrap_or(""),
                        b.candidate.release_id.as_deref().unwrap_or(""),
                    )
                })
        });

        let best = &ranked[0];
        let mut confidence = if best.distance <= Self::STRONG_THRESHOLD {
            TagConfidence::Strong
        } else if best.distance <= Self::MEDIUM_THRESHOLD {
            TagConfidence::Medium
        } else {
            TagConfidence::Low
        };

        if confidence != TagConfidence::Low && ranked.len() > 1 && Self::is_ambiguous(best, &ranked[1]) {
            confidence = TagConfidence::Ambiguous;
        }

        let note = (confidence == TagConfidence::Ambiguous).then(|| {
            format!(
                "two releases are too close to call: {} and {}; the album was left as it was",
                TagPlan::describe(&best.candidate),
                TagPlan::describe(&ranked[1].candidate)
            )
        });
        let mut plan = TagPlan {
            confidence,
            chosen: Some(best.clone()),
            ranked,
            evidence: Some(evidence.clone()),
            settings: settings.clone(),
            ..Default::default()
        };
        plan.notes.extend(note);
        plan
    }

    /// Two best candidates of different release groups, the same kind, within the margin.
    pub fn is_ambiguous(first: &ScoredCandidate, second: &ScoredCandidate) -> bool {
        if second.distance - first.distance > Self::AMBIGUITY_MARGIN {
            return false;
        }
        let a = first
            .candidate
            .release_group_id
            .as_deref()
            .filter(|id| !id.is_empty());
        let b = second
            .candidate
            .release_group_id
            .as_deref()
            .filter(|id| !id.is_empty());
        let different_group = match (a, b) {
            (Some(a), Some(b)) => !crate::common::dotnet::eq_ignore_case(a, b),
            _ => {
                SongIdentity::key(first.candidate.album_title().unwrap_or(""))
                    != SongIdentity::key(second.candidate.album_title().unwrap_or(""))
            }
        };
        if !different_group {
            return false;
        }
        first.penalty_of("type") == second.penalty_of("type")
    }
}

#[cfg(test)]
#[path = "release_chooser_tests.rs"]
mod tests;
