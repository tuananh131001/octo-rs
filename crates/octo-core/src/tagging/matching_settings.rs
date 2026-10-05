//! Port of `Services/Tagging/MatchingSettings.cs`.

use crate::settings::metadata::MetadataSettings;
use crate::settings::soulseek::SoulseekSettings;

/// The settings the chooser reads, snapshotted once per download.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchingSettings {
    pub prefer_original_album: bool,
    pub year_from_original_release: bool,
    pub preferred_countries: Vec<String>,
    pub fingerprint_threshold: f64,
    pub tag_from_match: bool,
}

impl Default for MatchingSettings {
    /// `MatchingSettings.Default`.
    fn default() -> Self {
        Self {
            prefer_original_album: true,
            year_from_original_release: true,
            preferred_countries: Vec::new(),
            fingerprint_threshold: 0.85,
            tag_from_match: false,
        }
    }
}

impl MatchingSettings {
    pub fn from_settings(metadata: &MetadataSettings, soulseek: &SoulseekSettings) -> Self {
        Self {
            prefer_original_album: metadata.prefer_original_album,
            year_from_original_release: metadata.year_from_original_release,
            preferred_countries: metadata.effective_preferred_countries(),
            fingerprint_threshold: soulseek.effective_min_score_fraction(),
            tag_from_match: soulseek.tag_from_music_brainz || soulseek.name_from_match,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_settings_reads_both_sections() {
        let metadata = MetadataSettings {
            prefer_original_album: false,
            ..Default::default()
        };
        let soulseek = SoulseekSettings {
            name_from_match: true,
            ..Default::default()
        };
        let settings = MatchingSettings::from_settings(&metadata, &soulseek);
        assert!(!settings.prefer_original_album);
        assert!(settings.year_from_original_release);
        assert!(settings.tag_from_match);
        assert_eq!(settings.fingerprint_threshold, 0.85);
        assert_eq!(MatchingSettings::default().fingerprint_threshold, 0.85);
    }
}
