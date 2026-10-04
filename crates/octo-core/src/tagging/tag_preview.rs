//! Port of `Services/Tagging/TagPreview.cs`.
//!
//! The services the C# resolved from the provider go through traits this crate defines:
//!
//! - [`FileFactsReader`] (`TagWriterExtras.ReadFacts`, `octo-media`, task 3-D);
//! - [`FingerprintVerifier`] (`DownloadVerificationService.IsFingerprintingEnabled` and
//!   `VerifyAsync`, the `octo` crate, task 4-B);
//! - [`PreviewLoudnessMeter`] (`ILoudnessMeter.MeasureAsync` plus `ReplayGainTags.ForTrack`,
//!   `octo-media`'s `audio::loudness_meter`, already ported: an adapter wraps it);
//! - [`CatalogBlankFiller`] (`BaseDownloadService.FillBlanksFromCatalog`, task 4-B).

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::fingerprint::verification::VerificationResult;
use crate::metadata::deezer_metadata_service::FullTrackMeta;
use crate::models::domain::song::Song;
use crate::settings::SettingsStore;
use crate::tagging::album_tag_context::AlbumTagContext;
use crate::tagging::release_identifier::{Cancelled, FileFactsReader, ReleaseIdentifier};
use crate::tagging::tag_plan::{FieldDecision, TagReport};

/// The verification calls a preview makes (`DownloadVerificationService`).
#[async_trait]
pub trait FingerprintVerifier: Send + Sync {
    /// `IsFingerprintingEnabled`: verification is on and AcoustID has a key.
    fn is_fingerprinting_enabled(&self) -> bool;

    /// `VerifyAsync(path, artist, title)`, with no ISRC and without refusing live takes.
    async fn verify(&self, path: &str, artist: &str, title: &str) -> VerificationResult;
}

/// The ReplayGain texts `ReplayGainTags.ForTrack` gives for a measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayGainText {
    /// `GainText`: "-6.50 dB".
    pub gain_text: String,
    /// `PeakText`: "0.966051".
    pub peak_text: String,
}

/// What a loudness measurement says, for the report.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasuredLoudness {
    pub integrated_lufs: f64,
    pub true_peak_dbfs: f64,
    /// `ReplayGainTags.ForTrack(measured)`, None when it gives no tags.
    pub replay_gain: Option<ReplayGainText>,
}

/// `ILoudnessMeter.MeasureAsync`, read through `ReplayGainTags.ForTrack`. None when the meter
/// could not measure; an error is what the C# caught as an exception.
#[async_trait]
pub trait PreviewLoudnessMeter: Send + Sync {
    async fn measure(
        &self,
        path: &str,
        timeout_seconds: i32,
        ct: &CancellationToken,
    ) -> anyhow::Result<Option<MeasuredLoudness>>;
}

/// `BaseDownloadService.FillBlanksFromCatalog`: the old fill-the-blanks rules that run after a plan.
pub trait CatalogBlankFiller: Send + Sync {
    fn fill_blanks_from_catalog(&self, song: &mut Song, meta: Option<&FullTrackMeta>);
}

/// "Try it on a song": the whole identification a download gets, run on a library file (with
/// its fingerprint and loudness) or on a name alone, outside the download lock and without
/// writing anything. The report is the same one a download leaves in the fetched-songs log.
pub struct TagPreview {
    settings: Arc<SettingsStore>,
    identifier: Arc<ReleaseIdentifier>,
    facts: Arc<dyn FileFactsReader>,
    verification: Option<Arc<dyn FingerprintVerifier>>,
    meter: Option<Arc<dyn PreviewLoudnessMeter>>,
    catalog: Arc<dyn CatalogBlankFiller>,
}

impl TagPreview {
    /// `verification` and `meter` are None where the C# found no such service registered.
    pub fn new(
        settings: Arc<SettingsStore>,
        identifier: Arc<ReleaseIdentifier>,
        facts: Arc<dyn FileFactsReader>,
        verification: Option<Arc<dyn FingerprintVerifier>>,
        meter: Option<Arc<dyn PreviewLoudnessMeter>>,
        catalog: Arc<dyn CatalogBlankFiller>,
    ) -> Self {
        Self {
            settings,
            identifier,
            facts,
            verification,
            meter,
            catalog,
        }
    }

    pub async fn preview(
        &self,
        path: Option<&str>,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
        ct: &CancellationToken,
    ) -> Result<TagReport, Cancelled> {
        let metadata = self.settings.current().metadata.clone();
        let mut song = Song {
            artist: artist.map(str::trim).unwrap_or("").to_string(),
            title: title.map(str::trim).unwrap_or("").to_string(),
            album: album.map(str::trim).unwrap_or("").to_string(),
            ..Default::default()
        };
        let mut notes: Vec<String> = Vec::new();
        let mut loudness = None;

        if let Some(path) = path {
            // The file's own name and artist stand in for a request that gave none; its album is
            // the file's claim, not the request's, so the chooser weighs it rather than keeps it.
            let facts = self.facts.read_facts(path, true);
            if song.artist.is_empty() {
                song.artist = facts.artist.clone().unwrap_or_default();
            }
            if song.title.is_empty() {
                song.title = facts.title.clone().unwrap_or_default();
            }
            song.duration = (facts.duration_seconds > 0).then_some(facts.duration_seconds);

            match &self.verification {
                Some(verification) if verification.is_fingerprinting_enabled() => {
                    let verdict = verification.verify(path, &song.artist, &song.title).await;
                    song.verification = Some(Box::new(verdict));
                }
                _ => notes.push(
                    "download verification is off or has no key, so the fingerprint service was not asked"
                        .to_string(),
                ),
            }

            if metadata.replay_gain
                && let Some(meter) = &self.meter
            {
                let (meter, path, token) = (Arc::clone(meter), path.to_string(), ct.clone());
                let timeout = metadata.effective_replay_gain_timeout_seconds();
                loudness = Some(tokio::spawn(async move {
                    meter.measure(&path, timeout, &token).await
                }));
            }
        } else {
            notes.push("matched by name alone: no file, so no fingerprint and no loudness".to_string());
        }

        let request =
            ReleaseIdentifier::request_for(&song, &song.artist, &song.title, Some(&song.album), None);
        let mut plan = self
            .identifier
            .identify::<()>(
                &song,
                request,
                path.unwrap_or(""),
                path.is_some(),
                None::<&AlbumTagContext>,
                ct,
            )
            .await?;
        plan.notes.splice(0..0, notes);
        plan.apply_to(&mut song);
        self.catalog
            .fill_blanks_from_catalog(&mut song, plan.catalog_best.as_ref());

        if let Some(task) = loudness {
            let measured = match task.await {
                Ok(measured) => measured,
                Err(join) => Err(anyhow::anyhow!("the measurement failed: {join}")),
            };
            match measured {
                Ok(measured) => {
                    plan.integrated_lufs = measured.as_ref().map(|m| m.integrated_lufs);
                    plan.true_peak_dbfs = measured.as_ref().map(|m| m.true_peak_dbfs);
                    if let Some(tags) = measured.and_then(|m| m.replay_gain) {
                        plan.fields.insert(
                            "replayGain".to_string(),
                            FieldDecision::new(
                                Some(&format!("{}, peak {}", tags.gain_text, tags.peak_text)),
                                Some("Measured"),
                            ),
                        );
                    }
                }
                Err(error) => debug!("preview loudness failed for {}: {}", path.unwrap_or(""), error),
            }
        }
        Ok(plan.to_report())
    }
}

#[cfg(test)]
#[path = "tag_preview_tests.rs"]
mod tests;
