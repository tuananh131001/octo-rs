//! Port of `Services/Fingerprint/AudioFingerprinter.cs`: Chromaprint fingerprints via fpcalc.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::warn;

use super::net_format;
use super::tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FingerprintOutcome {
    /// fpcalc produced a fingerprint.
    Ok,

    /// fpcalc is not in the image, timed out, or failed in a way that says nothing about
    /// the FILE. Verification must treat this as "no opinion" and keep the download.
    Unavailable,

    /// fpcalc started, read the file, and could not decode audio out of it. That is a fact
    /// about the file: a zero-filled or truncated FLAC that TagLib still reports a duration
    /// for decodes to nothing here.
    Undecodable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintResult {
    pub outcome: FingerprintOutcome,
    pub fingerprint: Option<String>,
    pub decoded_seconds: i32,
}

impl FingerprintResult {
    pub fn new(outcome: FingerprintOutcome, fingerprint: Option<String>, decoded_seconds: i32) -> Self {
        Self {
            outcome,
            fingerprint,
            decoded_seconds,
        }
    }

    fn missing() -> Self {
        Self::new(FingerprintOutcome::Unavailable, None, 0)
    }
}

/// Chromaprint fingerprints, via the fpcalc binary.
///
/// Modelled on LastFmRadioAudioTranscoder's ffmpeg shell-out, with one deliberate
/// difference: that one translates a missing binary into an error, because radio
/// cannot work without ffmpeg. Here a missing binary must NEVER fail a download that has
/// already succeeded. Every failure path is "no opinion", and the caller keeps the file.
#[derive(Debug, Default)]
pub struct AudioFingerprinter {
    /// Latched so a misbuilt image costs one log line, not a spawned process per download.
    binary_missing: AtomicBool,
}

impl AudioFingerprinter {
    pub fn new() -> Self {
        Self::default()
    }

    /// How much audio to fingerprint and how long fpcalc may take both come from settings:
    /// the first trades decode time against nothing much, and the second depends entirely on
    /// how slow the user's storage is.
    pub async fn fingerprint(
        &self,
        path: &Path,
        length_seconds: i32,
        timeout_seconds: i32,
    ) -> FingerprintResult {
        if self.binary_missing.load(Ordering::Relaxed) {
            return FingerprintResult::missing();
        }

        let length = length_seconds.to_string();
        let path_text = path.to_string_lossy();
        let mut child = match tool::spawn("fpcalc", &["-json", "-length", &length, &path_text]) {
            Ok(child) => child,
            Err(error) => {
                self.binary_missing.store(true, Ordering::Relaxed);
                warn!(
                    "fpcalc is not in this image, so download verification can only ever accept: {error}. \
                     Rebuild with libchromaprint-tools installed."
                );
                return FingerprintResult::missing();
            }
        };

        // Deliberately not linked to any caller token. Verification runs between a finished
        // transfer and the file joining the library, and a caller who has already left must
        // not be able to wave an unchecked file through. (A negative timeout, which C# threw
        // on, times out at once here.)
        let timeout = Duration::from_secs(timeout_seconds.max(0).unsigned_abs() as u64);
        let run = match tokio::time::timeout(timeout, tool::collect(&mut child, true)).await {
            Ok(Ok(run)) => run,
            Ok(Err(error)) => {
                tool::kill(&mut child);
                warn!("fpcalc did not finish for {}: {error}", path.display());
                return FingerprintResult::missing();
            }
            Err(_) => {
                tool::kill(&mut child);
                warn!(
                    "fpcalc did not finish for {}: The operation was canceled.",
                    path.display()
                );
                return FingerprintResult::missing();
            }
        };

        let stdout = String::from_utf8_lossy(&run.stdout);
        if let Some(parsed) = Self::parse_fpcalc_json(Some(&stdout)) {
            return parsed;
        }

        // Exit 0 with nothing parseable is a shape we do not understand: no opinion.
        if run.exit_code == 0 {
            warn!("fpcalc produced no usable JSON for {}", path.display());
            return FingerprintResult::missing();
        }

        // Started, read the file, refused it. The file is the problem. This is the branch a
        // zero-filled FLAC lands in when its header still claims a runtime.
        warn!(
            "fpcalc could not decode {} (exit {}): {}",
            path.display(),
            run.exit_code,
            run.stderr.trim()
        );
        FingerprintResult::new(FingerprintOutcome::Undecodable, None, 0)
    }

    /// fpcalc -json writes one object to stdout: {"duration": 253.14, "fingerprint": "AQADt..."}.
    /// Simpler than the transcoder's stderr scraping, which is why -json is used.
    ///
    /// Returns None when the output is not fpcalc's shape at all, so the caller can tell
    /// "I do not understand this" apart from "this file has no audio".
    pub(crate) fn parse_fpcalc_json(stdout: Option<&str>) -> Option<FingerprintResult> {
        let stdout = stdout?;
        if stdout.trim().is_empty() {
            return None;
        }
        let root: serde_json::Value = serde_json::from_str(stdout).ok()?;
        let root = root.as_object()?;

        // A "fingerprint" that is not a string made C# throw from GetString(); here it is
        // "not fpcalc's shape" like any other.
        let fingerprint = match root.get("fingerprint") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(text)) => Some(text.clone()),
            Some(_) => return None,
        };
        let fingerprint = fingerprint.filter(|text| !text.trim().is_empty())?;

        let seconds = match root.get("duration") {
            Some(serde_json::Value::Number(number)) => number
                .as_f64()
                .map(|value| net_format::round(value) as i32)
                .unwrap_or(0),
            _ => 0,
        };

        // A fingerprint over zero seconds of audio is not a fingerprint of anything.
        if seconds <= 0 {
            return Some(FingerprintResult::new(FingerprintOutcome::Undecodable, None, 0));
        }

        Some(FingerprintResult::new(
            FingerprintOutcome::Ok,
            Some(fingerprint),
            seconds,
        ))
    }
}

#[cfg(test)]
mod tests {
    //! fpcalc's output is the only thing standing between a finished download and an AcoustID
    //! lookup, and every failure here has to mean "no opinion" rather than "reject", because a
    //! rejection deletes a file and writes a deny-list entry.

    use super::*;

    #[test]
    fn parse_fpcalc_json_real_output_reads_fingerprint_and_duration() {
        let result = AudioFingerprinter::parse_fpcalc_json(Some(
            r#"{"duration": 253.14, "fingerprint": "AQADtEmkRUkSHR2CH1-OHT-Ko8dxHT2OH0eP4-jxHT2O"}"#,
        ))
        .expect("fpcalc's shape");

        assert_eq!(result.outcome, FingerprintOutcome::Ok);
        assert_eq!(result.decoded_seconds, 253);
        assert!(
            result
                .fingerprint
                .as_deref()
                .is_some_and(|fp| fp.starts_with("AQADtEmkRUkSHR2CH1"))
        );
    }

    /// A fingerprint over zero seconds of audio is not a fingerprint of anything. This is the
    /// shape a zero-filled FLAC produces when its header still claims a runtime.
    #[test]
    fn parse_fpcalc_json_zero_duration_is_undecodable_not_ok() {
        let result =
            AudioFingerprinter::parse_fpcalc_json(Some(r#"{"duration": 0, "fingerprint": "AQADtEmk"}"#))
                .expect("fpcalc's shape");

        assert_eq!(result.outcome, FingerprintOutcome::Undecodable);
    }

    /// None means "this is not fpcalc's shape at all", which the caller must tell apart from
    /// "this file has no audio": one keeps the file, the other deletes it.
    #[test]
    fn parse_fpcalc_json_unrecognised_output_returns_null_rather_than_throwing() {
        let cases = [
            "",
            "   ",
            "not json at all",
            "[1,2,3]",
            r#"{"duration": 253.14}"#,
            r#"{"duration": 253.14, "fingerprint": ""}"#,
        ];
        for stdout in cases {
            assert_eq!(
                AudioFingerprinter::parse_fpcalc_json(Some(stdout)),
                None,
                "stdout {stdout:?}"
            );
        }
        assert_eq!(AudioFingerprinter::parse_fpcalc_json(None), None);
    }

    /// The contract under test is "verification never fails a download", not which flavour of
    /// failure a given machine produces: there may be no fpcalc on the machine and there is no
    /// audio file at this path on any of them.
    #[tokio::test]
    async fn fingerprint_async_missing_file_or_binary_never_throws_into_the_download_loop() {
        let fingerprinter = AudioFingerprinter::new();
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("octo-no-such-file.flac");

        let result = fingerprinter.fingerprint(&path, 120, 5).await;

        assert_ne!(result.outcome, FingerprintOutcome::Ok);
    }

    // ---- Rust-only ----------------------------------------------------------------------

    /// A missing fpcalc is latched: the second call does not try to start it again. Skips when
    /// fpcalc is installed, since then nothing is latched.
    #[tokio::test]
    async fn a_missing_fpcalc_is_latched() {
        if which("fpcalc") {
            eprintln!("skipped: fpcalc is on the PATH, so there is nothing to latch");
            return;
        }
        let fingerprinter = AudioFingerprinter::new();
        let path = std::env::temp_dir().join("octo-no-such-file.flac");
        assert_eq!(
            fingerprinter.fingerprint(&path, 120, 5).await.outcome,
            FingerprintOutcome::Unavailable
        );
        assert!(fingerprinter.binary_missing.load(Ordering::Relaxed));
        assert_eq!(
            fingerprinter.fingerprint(&path, 120, 5).await.outcome,
            FingerprintOutcome::Unavailable
        );
    }

    /// Real fpcalc on a real tone. Skips cleanly where fpcalc is not installed.
    #[tokio::test]
    async fn fpcalc_fingerprints_a_tone() {
        if !which("fpcalc") {
            eprintln!("skipped: fpcalc is not on the PATH");
            return;
        }
        crate::audio::test_support::require_ffmpeg!();
        let dir = tempfile::tempdir().expect("a temp dir");
        crate::audio::test_support::run_ffmpeg(
            dir.path(),
            "-f lavfi -i anoisesrc=color=pink:duration=12:sample_rate=44100:amplitude=0.3 -c:a flac noise.flac",
        );
        let result = AudioFingerprinter::new()
            .fingerprint(&dir.path().join("noise.flac"), 120, 30)
            .await;
        assert_eq!(result.outcome, FingerprintOutcome::Ok, "{result:?}");
        assert_eq!(result.decoded_seconds, 12);
    }

    fn which(program: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
    }
}
