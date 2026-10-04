//! The ffmpeg and fpcalc tools: loudness for ReplayGain, Chromaprint fingerprints, and the
//! spectrum check that tells a fake lossless file from a real one.
//!
//! C# sources: `Services/Audio/LoudnessMeter.cs`, `Services/Fingerprint/AudioFingerprinter.cs`
//! and `Services/Fingerprint/SpectrumAnalyzer.cs`. Each one shells out to a binary that may be
//! missing from a misbuilt image, and each one degrades to "no opinion" when it is, latching the
//! fact after one warning.

pub mod audio_fingerprinter;
pub mod loudness_meter;
pub mod spectrum_analyzer;

mod net_format;
mod tool;

pub use audio_fingerprinter::{AudioFingerprinter, FingerprintOutcome, FingerprintResult};
pub use loudness_meter::{ILoudnessMeter, Loudness, LoudnessMeter, ReplayGainTags};
pub use spectrum_analyzer::{SpectrumAnalyzer, SpectrumReport, SpectrumVerdict};

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    /// Whether ffmpeg is on the PATH, asked once, as `FfmpegFactAttribute` did.
    pub(crate) fn ffmpeg_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            Command::new("ffmpeg")
                .args(["-hide_banner", "-version"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
    }

    /// Returns early from a test, with the reason printed, when ffmpeg is missing.
    macro_rules! require_ffmpeg {
        () => {
            if !$crate::audio::test_support::ffmpeg_available() {
                eprintln!("skipped: ffmpeg is not on the PATH");
                return;
            }
        };
    }
    pub(crate) use require_ffmpeg;

    /// Runs `ffmpeg -y -nostdin -hide_banner -v error <arguments>` in `dir`, panicking with
    /// ffmpeg's own error when it fails (the fixture's `Run`).
    pub(crate) fn run_ffmpeg(dir: &Path, arguments: &str) {
        let output = Command::new("ffmpeg")
            .args(["-y", "-nostdin", "-hide_banner", "-v", "error"])
            .args(arguments.split_whitespace())
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .expect("ffmpeg starts");
        assert!(
            output.status.success(),
            "ffmpeg {arguments}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
