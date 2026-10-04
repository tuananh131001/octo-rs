//! Port of `Services/Audio/LoudnessMeter.cs`: the loudness of a file by ffmpeg's EBU R128
//! filter, and the ReplayGain values that follow from it.

use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::net_format;
use super::tool;

/// What one file sounds like by the loudness standard: its integrated loudness, its
/// loudness range and its true peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub integrated_lufs: f64,
    pub loudness_range_lu: f64,
    pub true_peak_dbfs: f64,
}

impl Loudness {
    pub fn new(integrated_lufs: f64, loudness_range_lu: f64, true_peak_dbfs: f64) -> Self {
        Self {
            integrated_lufs,
            loudness_range_lu,
            true_peak_dbfs,
        }
    }
}

/// ReplayGain values for one file: the gain that brings it to the reference level and its peak
/// as a fraction of full scale. None when the measurement was silence or damage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayGainTags {
    pub gain_db: f64,
    pub peak: f64,
    pub integrated_lufs: f64,
}

impl ReplayGainTags {
    /// The level ReplayGain 2.0 brings every track to.
    pub const REFERENCE_LUFS: f64 = -18.0;

    /// The most a gain may be in either direction; beyond it the measurement is of
    /// silence or damage, not a quiet master.
    pub const MAX_GAIN_DB: f64 = 24.0;

    /// The level Opus files are normalised to, with the gain stored in 1/256 dB.
    const OPUS_REFERENCE_LUFS: f64 = -23.0;

    pub fn new(gain_db: f64, peak: f64, integrated_lufs: f64) -> Self {
        Self {
            gain_db,
            peak,
            integrated_lufs,
        }
    }

    pub fn for_track(loudness: Option<&Loudness>) -> Option<Self> {
        let loudness = loudness?;
        if !loudness.integrated_lufs.is_finite() {
            return None;
        }
        let gain = net_format::round_digits(
            (Self::REFERENCE_LUFS - loudness.integrated_lufs).clamp(-Self::MAX_GAIN_DB, Self::MAX_GAIN_DB),
            2,
        );
        let peak_db = if loudness.true_peak_dbfs.is_finite() {
            loudness.true_peak_dbfs
        } else {
            -100.0
        };
        let peak = net_format::round_digits(10f64.powf(peak_db / 20.0), 6);
        Some(Self::new(gain, peak, loudness.integrated_lufs))
    }

    /// The album's values from its tracks: the gain for the album's loudness as a whole, taken
    /// as the power mean of the tracks' integrated loudness, and the loudest peak. None when
    /// any track could not be measured, since an album gain for half an album is worse than none.
    pub fn for_album(tracks: &[Option<Loudness>]) -> Option<Self> {
        if tracks.is_empty() || tracks.iter().any(Option::is_none) {
            return None;
        }
        let measured: Vec<Loudness> = tracks.iter().flatten().copied().collect();
        if measured.iter().any(|t| !t.integrated_lufs.is_finite()) {
            return None;
        }
        let power = measured
            .iter()
            .map(|t| 10f64.powf(t.integrated_lufs / 10.0))
            .sum::<f64>()
            / measured.len() as f64;
        let integrated = 10.0 * power.log10();
        let peak = measured
            .iter()
            .map(|t| {
                if t.true_peak_dbfs.is_finite() {
                    t.true_peak_dbfs
                } else {
                    -100.0
                }
            })
            .fold(f64::NEG_INFINITY, f64::max);
        Self::for_track(Some(&Loudness::new(integrated, 0.0, peak)))
    }

    /// The gain as players read it: `"+0.00;-0.00"` plus `" dB"`, with a dot whatever the
    /// culture. Zero takes the positive section.
    pub fn gain_text(&self) -> String {
        let body = net_format::fixed(self.gain_db.abs(), 2, 2);
        let sign = if self.gain_db < 0.0 && body != "0.00" {
            '-'
        } else {
            '+'
        };
        format!("{sign}{body} dB")
    }

    /// The peak with six decimals (`"0.000000"`).
    pub fn peak_text(&self) -> String {
        net_format::fixed(self.peak, 6, 6)
    }

    /// The gain an Opus file carries, relative to -23 LUFS in 1/256 dB steps.
    pub fn opus_gain(&self) -> i32 {
        net_format::round((Self::OPUS_REFERENCE_LUFS - self.integrated_lufs) * 256.0) as i32
    }
}

#[async_trait]
pub trait ILoudnessMeter: Send + Sync {
    /// Measure one file. None when ffmpeg is missing, the file will not decode, or the
    /// time ran out; never a panic, never a failed download.
    async fn measure(&self, path: &Path, timeout_seconds: i32, ct: &CancellationToken) -> Option<Loudness>;
}

static LOUDNESS_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(I|LRA|Peak):\s+(-?[0-9.]+|-inf|inf)\s+(LUFS|LU|dBFS)")
        .expect("the loudness line pattern is valid")
});

/// Measures a file's loudness with ffmpeg, the way the radio transcoder does: decode to one
/// fixed format, run the loudness filter with true peak on, and read the summary it prints on
/// stderr. A missing ffmpeg is latched after one warning, so a misbuilt image costs a log line
/// and not a spawned process per download.
#[derive(Debug, Default)]
pub struct LoudnessMeter {
    binary_missing: AtomicBool,
}

impl LoudnessMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The summary block the loudness filter prints at the end of its stderr. An error is a
    /// level the pattern let through that is still not a number (`1.2.3`), which C# threw
    /// from `double.Parse`. Public because the radio transcoder reads the same summary.
    pub fn parse(report: &str) -> Result<Option<Loudness>, std::num::ParseFloatError> {
        let (mut integrated, mut range, mut peak) = (None, None, None);
        for captures in LOUDNESS_LINE.captures_iter(report) {
            let value = parse_level(&captures[2])?;
            match &captures[1] {
                "I" => integrated = Some(value),
                "LRA" => range = Some(value),
                "Peak" => peak = Some(value),
                _ => {}
            }
        }
        Ok(integrated.map(|integrated| Loudness::new(integrated, range.unwrap_or(0.0), peak.unwrap_or(0.0))))
    }
}

fn parse_level(text: &str) -> Result<f64, std::num::ParseFloatError> {
    match text {
        "-inf" => Ok(f64::NEG_INFINITY),
        "inf" => Ok(f64::INFINITY),
        _ => text.parse(),
    }
}

#[async_trait]
impl ILoudnessMeter for LoudnessMeter {
    async fn measure(&self, path: &Path, timeout_seconds: i32, ct: &CancellationToken) -> Option<Loudness> {
        if self.binary_missing.load(Ordering::Relaxed) || path.as_os_str().is_empty() || !path.is_file() {
            return None;
        }

        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1).unsigned_abs() as u64);

        let path_text = path.to_string_lossy();
        let arguments = [
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-i",
            &path_text,
            "-vn",
            "-af",
            "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo,ebur128=peak=true",
            "-f",
            "null",
            "-",
        ];
        let mut child = match tool::spawn("ffmpeg", &arguments) {
            Ok(child) => child,
            Err(error) => {
                self.binary_missing.store(true, Ordering::Relaxed);
                warn!("ffmpeg is not in this image, so downloads get no ReplayGain: {error}");
                return None;
            }
        };

        let outcome = tokio::select! {
            biased;
            _ = ct.cancelled() => None,
            _ = tokio::time::sleep_until(deadline) => None,
            run = tool::collect(&mut child, false) => Some(run),
        };
        match outcome {
            None => {
                tool::kill(&mut child);
                warn!(
                    "the loudness measurement of {} took longer than {timeout_seconds}s; no ReplayGain",
                    path.display()
                );
                None
            }
            Some(Err(error)) => {
                tool::kill(&mut child);
                debug!("the loudness measurement of {} failed: {error}", path.display());
                None
            }
            Some(Ok(run)) => match Self::parse(&run.stderr) {
                Ok(loudness) => {
                    if loudness.is_none() {
                        debug!(
                            "ffmpeg gave no loudness summary for {} (exit {})",
                            path.display(),
                            run.exit_code
                        );
                    }
                    loudness
                }
                Err(error) => {
                    debug!("the loudness measurement of {} failed: {error}", path.display());
                    None
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    //! The loudness summary ffmpeg prints, the ReplayGain maths from it, and the formats players
    //! read: a gain with two decimals and " dB", a peak with six, and a dot whatever the culture.

    use super::*;
    use crate::audio::test_support::{require_ffmpeg, run_ffmpeg};

    /// A real summary block, captured from ffmpeg 6 with ebur128=peak=true.
    const SUMMARY: &str = "[Parsed_ebur128_1 @ 0x55d0] Summary:

  Integrated loudness:
    I:         -11.5 LUFS
    Threshold: -21.8 LUFS

  Loudness range:
    LRA:         6.3 LU
    Threshold: -31.9 LUFS
    LRA low:   -15.6 LUFS
    LRA high:   -9.3 LUFS

  True peak:
    Peak:       -0.3 dBFS";

    fn parse(report: &str) -> Option<Loudness> {
        LoudnessMeter::parse(report).expect("every level is a number")
    }

    fn assert_close(expected: f64, actual: f64, decimals: i32) {
        assert_eq!(
            net_format::round_digits(expected, decimals),
            net_format::round_digits(actual, decimals),
            "expected {expected}, got {actual} (to {decimals} decimals)"
        );
    }

    #[test]
    fn parse_reads_integrated_range_and_true_peak() {
        let loudness = parse(SUMMARY).expect("a summary");
        assert_close(-11.5, loudness.integrated_lufs, 3);
        assert_close(6.3, loudness.loudness_range_lu, 3);
        assert_close(-0.3, loudness.true_peak_dbfs, 3);
    }

    #[test]
    fn parse_no_summary_is_null() {
        assert_eq!(parse("some other stderr"), None);
    }

    #[test]
    fn parse_silence_is_negative_infinity() {
        let loudness = parse("  I:         -inf LUFS\n  Peak:       -inf dBFS\n").expect("a summary");
        assert_eq!(loudness.integrated_lufs, f64::NEG_INFINITY);
    }

    #[test]
    fn for_track_gain_and_peak_from_the_summary() {
        let tags = ReplayGainTags::for_track(parse(SUMMARY).as_ref()).expect("tags");
        assert_close(-6.5, tags.gain_db, 3);
        assert_close(0.966051, tags.peak, 6);
        assert_eq!(tags.gain_text(), "-6.50 dB");
        assert_eq!(tags.peak_text(), "0.966051");
    }

    #[test]
    fn for_track_silence_or_nothing_is_null() {
        assert_eq!(ReplayGainTags::for_track(None), None);
        assert_eq!(
            ReplayGainTags::for_track(Some(&Loudness::new(f64::NEG_INFINITY, 0.0, f64::NEG_INFINITY))),
            None
        );
        assert_eq!(
            ReplayGainTags::for_track(Some(&Loudness::new(f64::NAN, 0.0, 0.0))),
            None
        );
    }

    #[test]
    fn for_track_gain_is_clamped_to_twenty_four_decibels() {
        assert_eq!(
            ReplayGainTags::for_track(Some(&Loudness::new(-70.0, 0.0, -40.0)))
                .unwrap()
                .gain_db,
            24.0
        );
        assert_eq!(
            ReplayGainTags::for_track(Some(&Loudness::new(20.0, 0.0, 3.0)))
                .unwrap()
                .gain_db,
            -24.0
        );
    }

    #[test]
    fn opus_gain_is_relative_to_minus23_in_steps_of256() {
        let tags = ReplayGainTags::for_track(Some(&Loudness::new(-11.5, 0.0, -0.3))).expect("tags");
        assert_eq!(tags.opus_gain(), net_format::round((-23.0 + 11.5) * 256.0) as i32);
        assert_eq!(tags.opus_gain(), -2944);
    }

    #[test]
    fn for_album_power_mean_of_the_tracks_and_the_loudest_peak() {
        let album = ReplayGainTags::for_album(&[
            Some(Loudness::new(-10.0, 0.0, -1.0)),
            Some(Loudness::new(-14.0, 0.0, -0.5)),
        ])
        .expect("album tags");
        let expected = 10.0 * ((10f64.powf(-1.0) + 10f64.powf(-1.4)) / 2.0).log10();
        assert_close(net_format::round_digits(-18.0 - expected, 2), album.gain_db, 3);
        assert_close(
            net_format::round_digits(10f64.powf(-0.5 / 20.0), 6),
            album.peak,
            6,
        );
    }

    #[test]
    fn for_album_any_unmeasured_track_gives_no_album_gain() {
        assert_eq!(
            ReplayGainTags::for_album(&[Some(Loudness::new(-10.0, 0.0, -1.0)), None]),
            None
        );
        assert_eq!(ReplayGainTags::for_album(&[]), None);
    }

    /// A server in a comma-decimal locale must still write a dot. Rust formatting has no
    /// culture, so this pins the invariant texts.
    #[test]
    fn formats_use_a_dot_whatever_the_culture() {
        let tags = ReplayGainTags::new(-6.5, 0.966051, -11.5);
        assert_eq!(tags.gain_text(), "-6.50 dB");
        assert_eq!(tags.peak_text(), "0.966051");
    }

    #[tokio::test]
    async fn measure_missing_file_is_null() {
        let meter = LoudnessMeter::new();
        let path = std::env::temp_dir().join("octo-no-such-file.flac");
        assert_eq!(meter.measure(&path, 5, &CancellationToken::new()).await, None);
    }

    // ---- Rust-only: the real ffmpeg command line ------------------------------------------

    /// Zero and positive gains take the "+" section.
    #[test]
    fn gain_text_signs() {
        assert_eq!(ReplayGainTags::new(0.0, 1.0, -18.0).gain_text(), "+0.00 dB");
        assert_eq!(ReplayGainTags::new(-0.0, 1.0, -18.0).gain_text(), "+0.00 dB");
        assert_eq!(ReplayGainTags::new(3.25, 1.0, -21.25).gain_text(), "+3.25 dB");
    }

    #[test]
    fn a_level_that_is_not_a_number_is_an_error() {
        assert!(LoudnessMeter::parse("  I:   1.2.3 LUFS\n").is_err());
    }

    /// Runs the exact command line on a generated tone and reads a sane summary back.
    #[tokio::test]
    async fn measure_a_tone_reads_the_summary() {
        require_ffmpeg!();
        let dir = tempfile::tempdir().expect("a temp dir");
        run_ffmpeg(
            dir.path(),
            "-f lavfi -i sine=frequency=1000:duration=5:sample_rate=44100 -c:a flac tone.flac",
        );
        let meter = LoudnessMeter::new();
        let loudness = meter
            .measure(&dir.path().join("tone.flac"), 30, &CancellationToken::new())
            .await
            .expect("ffmpeg measured the tone");
        // ffmpeg's sine source is at 1/8 of full scale (-18 dBFS); the mono-to-stereo
        // aformat takes 3 dB off each channel, so ffmpeg reports about -21.1 for both.
        assert!((-24.0..-18.0).contains(&loudness.integrated_lufs), "{loudness:?}");
        assert!((-24.0..-18.0).contains(&loudness.true_peak_dbfs), "{loudness:?}");
    }

    /// A caller that has already given up gets nothing, and the process is not waited on.
    #[tokio::test]
    async fn measure_cancelled_is_null() {
        require_ffmpeg!();
        let dir = tempfile::tempdir().expect("a temp dir");
        run_ffmpeg(
            dir.path(),
            "-f lavfi -i sine=frequency=1000:duration=5:sample_rate=44100 -c:a flac tone.flac",
        );
        let ct = CancellationToken::new();
        ct.cancel();
        let meter = LoudnessMeter::new();
        assert_eq!(meter.measure(&dir.path().join("tone.flac"), 30, &ct).await, None);
    }
}
