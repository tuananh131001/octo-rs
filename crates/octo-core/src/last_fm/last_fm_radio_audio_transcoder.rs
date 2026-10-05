//! The pure half of `Services/LastFm/LastFmRadioAudioTranscoder.cs`: the measured profile a
//! cached radio track carries (`<cache>/radio/<key>.mp3.json`), the loudness gain, and the
//! reading of ffmpeg's spectral statistics. The ffmpeg runs are
//! `octo::services::last_fm::last_fm_radio_audio_transcoder`.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::common::dotnet;

/// What one radio track sounds like and what it is, gathered while it was prepared. Loudness
/// fields describe the SOURCE; `gain_db` is what was applied to reach the target. The spectral
/// fields are means over the track and describe its character (brightness, noisiness,
/// bandwidth), which survive the gain unchanged. `genre` is the catalogue genre the track
/// resolved with and `tags` its Last.fm top tags (sub-genre), so the flow picker can judge
/// kinship alongside sound rather than by sound alone.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RadioAudioProfile {
    #[serde(default)]
    pub integrated_lufs: f64,
    #[serde(default)]
    pub loudness_range_lu: f64,
    #[serde(default)]
    pub true_peak_dbfs: f64,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default)]
    pub spectral_centroid_hz: f64,
    #[serde(default)]
    pub spectral_flatness: f64,
    #[serde(default)]
    pub spectral_rolloff_hz: f64,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

impl RadioAudioProfile {
    /// The positional record's constructor, without the optional genre and tags.
    pub fn new(
        integrated_lufs: f64,
        loudness_range_lu: f64,
        true_peak_dbfs: f64,
        gain_db: f64,
        spectral_centroid_hz: f64,
        spectral_flatness: f64,
        spectral_rolloff_hz: f64,
    ) -> Self {
        RadioAudioProfile {
            integrated_lufs,
            loudness_range_lu,
            true_peak_dbfs,
            gain_db,
            spectral_centroid_hz,
            spectral_flatness,
            spectral_rolloff_hz,
            genre: None,
            tags: None,
        }
    }

    /// `with { Genre = .., Tags = .. }`.
    pub fn with_kinship(mut self, genre: Option<&str>, tags: Option<Vec<String>>) -> Self {
        self.genre = genre.map(str::to_string);
        self.tags = tags;
        self
    }
}

/// Largest static gain applied in either direction. Anything further out is almost certainly a
/// measurement of silence or damage, not a quiet master.
pub const MAXIMUM_GAIN_DB: f64 = 18.0;

/// -1 dBTP as a linear limit for the true-peak limiter.
pub const LIMITER_CEILING: &str = "0.891251";

/// The static gain that takes a measured loudness to the target, bounded. No target, or a
/// measurement that is not a number (silence reads as -inf), means no gain.
pub fn gain_for(measured_lufs: Option<f64>, target_lufs: Option<f64>) -> f64 {
    let (Some(target), Some(measured)) = (target_lufs, measured_lufs) else {
        return 0.0;
    };
    if measured.is_nan() || measured.is_infinite() {
        return 0.0;
    }
    // Math.Clamp(NaN, ..) is NaN, and Math.Round keeps it: a NaN target gives no gain only
    // through the test's own reading of NaN as "no target".
    dotnet::round((target - measured).clamp(-MAXIMUM_GAIN_DB, MAXIMUM_GAIN_DB), 2)
}

/// The volume filter's text for a gain: `"0.00"` in the invariant culture.
pub fn gain_text(gain: f64) -> String {
    format!("{gain:.2}")
}

static SPECTRAL_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?mi)^lavfi\.aspectralstats\.1\.(centroid|flatness|rolloff)=(-?[0-9.]+(?:e[-+]?\d+)?)\s*$")
        .expect("the spectral pattern compiles")
});

/// The means of the centroid, flatness and rolloff lines `ametadata=mode=print` wrote, each 0
/// when none was finite. An error is a value the pattern let through that is not a number
/// (`1.2.3`), which C# threw from `double.Parse`.
pub fn read_spectral(text: &str) -> Result<(f64, f64, f64), std::num::ParseFloatError> {
    let (mut centroid, mut flatness, mut rolloff) = (0.0, 0.0, 0.0);
    let (mut centroids, mut flatnesses, mut rolloffs) = (0, 0, 0);
    for captures in SPECTRAL_LINE.captures_iter(text) {
        let value: f64 = captures[2].parse()?;
        if value.is_nan() || value.is_infinite() {
            continue;
        }
        match captures[1].to_ascii_lowercase().as_str() {
            "centroid" => {
                centroid += value;
                centroids += 1;
            }
            "flatness" => {
                flatness += value;
                flatnesses += 1;
            }
            "rolloff" => {
                rolloff += value;
                rolloffs += 1;
            }
            _ => {}
        }
    }
    let mean = |sum: f64, count: i32| if count == 0 { 0.0 } else { sum / f64::from(count) };
    Ok((
        mean(centroid, centroids),
        mean(flatness, flatnesses),
        mean(rolloff, rolloffs),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // LastFmRadioCoreTests.Gain_TakesTheMeasurementToTheTargetWithinBounds
    #[test]
    fn gain_takes_the_measurement_to_the_target_within_bounds() {
        let cases = [
            (Some(-10.7), Some(-16.0), -5.3),
            (Some(-24.0), Some(-16.0), 8.0),
            (Some(-40.0), Some(-16.0), 18.0),
            (Some(-10.0), None, 0.0),
            (None, Some(-16.0), 0.0),
            (Some(f64::NEG_INFINITY), Some(-16.0), 0.0),
        ];
        for (measured, target, expected) in cases {
            let gain = gain_for(measured, target);
            assert!(
                (gain - expected).abs() < 0.005,
                "{measured:?} -> {target:?}: {gain}"
            );
        }
    }

    #[test]
    fn gain_text_has_two_decimals() {
        assert_eq!(gain_text(-5.3), "-5.30");
        assert_eq!(gain_text(18.0), "18.00");
    }

    /// The radio cache fixture reads and writes back byte for byte (state-files.md §4.25).
    #[test]
    fn the_profile_fixture_round_trips_byte_for_byte() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/cache/radio/3f79bb7b435b05321651daefd374cdc681dc06faa65e374e38337b88ca046dea.mp3.json"
        );
        let text = std::fs::read_to_string(path).expect("the fixture is in the repo");
        let profile: RadioAudioProfile = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(profile.genre.as_deref(), Some("Electronic"));
        assert_eq!(profile.tags.as_ref().map(Vec::len), Some(3));
        assert_eq!(crate::json::to_string(&profile), text);
    }

    #[test]
    fn the_spectral_means_skip_what_is_not_finite() {
        let text = "frame:0    pts:0       pts_time:0\n\
                    lavfi.aspectralstats.1.centroid=1000.5\n\
                    lavfi.aspectralstats.1.flatness=0.1\n\
                    lavfi.aspectralstats.1.rolloff=5000\n\
                    lavfi.aspectralstats.1.mean=3\n\
                    frame:1    pts:4096    pts_time:0.09\n\
                    LAVFI.ASPECTRALSTATS.1.CENTROID=2000.5 \n\
                    lavfi.aspectralstats.1.flatness=0.3\n";
        assert_eq!(read_spectral(text), Ok((1500.5, 0.2, 5000.0)));
        assert_eq!(read_spectral(""), Ok((0.0, 0.0, 0.0)));
        assert!(read_spectral("lavfi.aspectralstats.1.centroid=1.2.3").is_err());
    }
}
