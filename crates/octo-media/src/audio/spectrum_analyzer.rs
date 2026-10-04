//! Port of `Services/Fingerprint/SpectrumAnalyzer.cs`: tells a lossless file made from a lossy
//! one by its spectrum.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lofty::file::AudioFile;
use tracing::{debug, warn};

use super::net_format;
use super::tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpectrumVerdict {
    /// No opinion: ffmpeg is missing, timed out or could not decode the file, the file is too
    /// short or too quiet, its sample rate is under 44.1 kHz, or its spectrum has no clear cliff.
    /// Treated exactly like a genuine file by every caller.
    Unknown,

    /// The audio reaches the top of the band, or stops at a cliff no lossy encoder uses.
    Genuine,

    /// The spectrum stops dead at a frequency a lossy encoder cuts at and stays at the floor
    /// above it: a lossless file made from an MP3 or an AAC. Still the right song, only not
    /// lossless.
    LikelyLossy,
}

/// What the spectrum of a file showed, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumReport {
    pub verdict: SpectrumVerdict,
    pub sample_rate: i32,
    /// Where the audio stops, when a cliff was found.
    pub cutoff_hz: Option<f64>,
    pub reason: String,
    /// For a likely lossy file, the bitrate its cutoff is typical of, in words
    /// ("about 128 kbps MP3").
    pub estimate: Option<String>,
}

impl SpectrumReport {
    pub fn new(
        verdict: SpectrumVerdict,
        sample_rate: i32,
        cutoff_hz: Option<f64>,
        reason: impl Into<String>,
        estimate: Option<String>,
    ) -> Self {
        Self {
            verdict,
            sample_rate,
            cutoff_hz,
            reason: reason.into(),
            estimate,
        }
    }

    pub fn is_likely_lossy(&self) -> bool {
        self.verdict == SpectrumVerdict::LikelyLossy
    }

    /// No opinion, for `reason`. C# defaulted `sampleRate` to 0; pass 0 for "not known".
    pub fn unknown(reason: impl Into<String>, sample_rate: i32) -> Self {
        Self::new(SpectrumVerdict::Unknown, sample_rate, None, reason, None)
    }

    /// One line for a log or a download record.
    pub fn describe(&self) -> String {
        match self.verdict {
            SpectrumVerdict::LikelyLossy => format!(
                "likely transcoded from {} (cutoff {} kHz)",
                self.estimate.as_deref().unwrap_or(""),
                // A null cutoff formatted as nothing in the C# interpolation.
                self.cutoff_hz
                    .map(|hz| net_format::fixed(hz / 1000.0, 1, 1))
                    .unwrap_or_default()
            ),
            SpectrumVerdict::Genuine => format!("genuine ({})", self.reason),
            SpectrumVerdict::Unknown => format!("no opinion ({})", self.reason),
        }
    }
}

/// Extensions that promise lossless audio. ALAC usually arrives as .m4a, which
/// also holds AAC, so it is not in the list: an m4a is never assumed to be lossless.
const LOSSLESS_EXTENSIONS: [&str; 7] = [".flac", ".wav", ".aiff", ".aif", ".alac", ".ape", ".wv"];

/// How many windows are decoded, spread through the track, and how long each is.
/// Four windows of six seconds reach past a quiet intro and a fade without decoding the
/// whole file.
pub(crate) const WINDOWS: usize = 4;
pub(crate) const WINDOW_SECONDS: f64 = 6.0;

/// Frames quieter than this (RMS, dB below full scale) are skipped as silence.
const SILENCE_DB: f64 = -65.0;

/// Fewer non-silent frames than this (about two seconds) is too little to judge.
pub(crate) const MIN_ACTIVE_FRAMES: usize = 40;

/// How far the level must fall across a cliff, in dB, measured between the half
/// kilohertz below it and the half kilohertz above it. A gentle roll-off at 24 dB per octave
/// loses about 2 dB over that distance; an encoder's cutoff loses 40 or more.
pub(crate) const MIN_CLIFF_DB: f64 = 30.0;

/// How far anything above the cliff may rise over the level just past it. More than
/// this and there is audio above the "cliff", so it was a dip, not a cutoff.
const MAX_TAIL_RISE_DB: f64 = 8.0;

/// A cliff at or above this is the top of a genuine recording's band, not an encoder's
/// cutoff. MP3 at 320 kbps stops at about 20.4 kHz; a CD master's anti-alias filter, and a
/// good resampler bringing a 48 kHz master down to 44.1, stop at 21 kHz or above.
pub(crate) const GENUINE_CUTOFF_HZ: f64 = 20700.0;

/// With no cliff, the top of the band must still carry audio within this many dB of
/// the midrange for the file to count as genuine rather than unknown. A 16-bit file's own
/// noise floor sits about 70 dB under loud music, so this stays well clear of it.
const TOP_BAND_CONTENT_DB: f64 = 45.0;

/// Tells a lossless file made from a lossy one ("fake FLAC") by its spectrum.
///
/// A lossy encoder throws away everything above a frequency it picks from its bitrate: about
/// 16 to 17 kHz at 128 kbps, 18.5 at 192, 19.5 to 20.5 at 256 and 320. Converting the result to
/// FLAC keeps that hole. So a few windows of the file are decoded to mono, the power spectrum is
/// averaged over every frame that is not silent, and the highest frequency where the level
/// falls off a cliff and stays at the floor all the way up is the cutoff.
///
/// The cliff is what makes the verdict, not a lack of treble. An old recording, a lo-fi mix or a
/// quiet acoustic track can have almost nothing above 15 kHz and still be genuine; its spectrum
/// slopes down, it does not stop. So a drop has to be steep (tens of dB inside about a
/// kilohertz) and the band above it has to stay flat, and a spectrum that merely fades is
/// Unknown. Unknown is always preferred to a wrong LikelyLossy, because a wrong one makes Octo
/// throw away a perfectly good download to go looking for another.
///
/// Process handling is modelled on AudioFingerprinter: a missing ffmpeg, a timeout or a file
/// that will not decode is no opinion, never a failed download.
#[derive(Debug, Default)]
pub struct SpectrumAnalyzer {
    /// Latched so a misbuilt image costs one log line, not a spawned process per file.
    binary_missing: AtomicBool,
}

/// Why decoding a window stopped short of samples.
enum DecodeError {
    /// ffmpeg did not start (C#'s private `FfmpegMissingException`).
    FfmpegMissing,
    /// Reading the pipes or waiting for the exit failed. C# let this escape `AnalyzeAsync`.
    Io(io::Error),
}

impl SpectrumAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a file's extension says it is lossless, which is the claim this checks.
    pub fn claims_lossless(path: &str) -> bool {
        !path.is_empty() && {
            let extension = net_format::get_extension(path);
            LOSSLESS_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        }
    }

    /// Decode a few windows of the file and judge its spectrum. Never fails; every failure is
    /// Unknown. The timeout covers the whole analysis, every window included.
    pub async fn analyze(&self, path: &Path, timeout_seconds: i32) -> SpectrumReport {
        if self.binary_missing.load(Ordering::Relaxed) {
            return SpectrumReport::unknown("ffmpeg is not available", 0);
        }

        let owned: PathBuf = path.to_path_buf();
        let format = tokio::task::spawn_blocking(move || -> Result<(i32, f64), String> {
            let file = lofty::read_from_path(&owned).map_err(|error| error.to_string())?;
            let properties = file.properties();
            Ok((
                properties.sample_rate().unwrap_or(0) as i32,
                properties.duration().as_secs_f64(),
            ))
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        let (sample_rate, seconds) = match format {
            Ok(format) => format,
            Err(message) => {
                debug!("could not read the format of {}: {message}", path.display());
                return SpectrumReport::unknown("the file's format could not be read", 0);
            }
        };

        // Below 44.1 kHz the band ends before the cutoffs this looks for, so there is nothing
        // to tell apart.
        if sample_rate < 44100 {
            return SpectrumReport::unknown(
                format!("a sample rate of {sample_rate} Hz is under 44.1 kHz"),
                sample_rate,
            );
        }
        if seconds < 3.0 {
            return SpectrumReport::unknown("too short", sample_rate);
        }

        let timeout = Duration::from_secs(timeout_seconds.max(1).unsigned_abs() as u64);
        let decode_all = async {
            let mut windows = Vec::new();
            for start in window_starts(seconds) {
                match self
                    .decode(path, start, WINDOW_SECONDS.min(seconds), sample_rate)
                    .await?
                {
                    Some(samples) => windows.push(samples),
                    None => return Ok(None),
                }
            }
            Ok::<_, DecodeError>(Some(windows))
        };
        // On a timeout the running ffmpeg is dropped with the future, which kills it.
        let windows = match tokio::time::timeout(timeout, decode_all).await {
            Ok(Ok(Some(windows))) => windows,
            Ok(Ok(None)) => return SpectrumReport::unknown("ffmpeg could not decode the file", sample_rate),
            Ok(Err(DecodeError::FfmpegMissing)) => {
                return SpectrumReport::unknown("ffmpeg is not available", 0);
            }
            Ok(Err(DecodeError::Io(error))) => {
                warn!("the spectrum check of {} failed: {error}", path.display());
                return SpectrumReport::unknown("the check failed", sample_rate);
            }
            Err(_) => {
                warn!(
                    "the spectrum check of {} took longer than {timeout_seconds}s; no opinion",
                    path.display()
                );
                return SpectrumReport::unknown("the check timed out", sample_rate);
            }
        };

        // The FFTs are CPU work, so they leave the async threads; a panic in them is C#'s
        // "the check failed" catch.
        match tokio::task::spawn_blocking(move || judge(&windows, sample_rate)).await {
            Ok(report) => report,
            Err(error) => {
                warn!("the spectrum check of {} failed: {error}", path.display());
                SpectrumReport::unknown("the check failed", sample_rate)
            }
        }
    }

    /// One window as mono 32-bit float samples at the file's own rate, or None when
    /// ffmpeg ran and could not decode it.
    async fn decode(
        &self,
        path: &Path,
        start: f64,
        length: f64,
        sample_rate: i32,
    ) -> Result<Option<Vec<f32>>, DecodeError> {
        let start_text = net_format::fixed(start, 0, 3);
        let length_text = net_format::fixed(length, 0, 3);
        let rate_text = sample_rate.to_string();
        let path_text = path.to_string_lossy();
        let arguments = [
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-ss",
            &start_text,
            "-t",
            &length_text,
            "-i",
            &path_text,
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            &rate_text,
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-",
        ];
        let mut child = match tool::spawn("ffmpeg", &arguments) {
            Ok(child) => child,
            Err(error) => {
                self.binary_missing.store(true, Ordering::Relaxed);
                warn!(
                    "ffmpeg is not in this image, so lossless downloads cannot be checked for transcoding: {error}"
                );
                return Err(DecodeError::FfmpegMissing);
            }
        };

        let run = match tool::collect(&mut child, true).await {
            Ok(run) => run,
            Err(error) => {
                tool::kill(&mut child);
                return Err(DecodeError::Io(error));
            }
        };

        if run.exit_code != 0 || run.stdout.len() < 4 {
            debug!(
                "ffmpeg could not decode {} for the spectrum check (exit {}): {}",
                path.display(),
                run.exit_code,
                run.stderr.trim()
            );
            return Ok(None);
        }

        let samples = run
            .stdout
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        Ok(Some(samples))
    }
}

/// Where each window starts: spread through the track, clear of the very start
/// and end, which are the likeliest to be silence or a fade.
pub(crate) fn window_starts(seconds: f64) -> Vec<f64> {
    if seconds <= WINDOW_SECONDS {
        return vec![0.0];
    }
    let mut starts: Vec<f64> = Vec::new();
    for i in 0..WINDOWS {
        let at = seconds * (0.15 + 0.2 * i as f64);
        let start = 0f64.max(at.min(seconds - WINDOW_SECONDS));
        // Distinct(), keeping the first of each.
        if !starts.contains(&start) {
            starts.push(start);
        }
    }
    starts
}

// ---- the spectrum -------------------------------------------------------------------

/// The FFT length for a sample rate: about 11 Hz a bin whatever the rate.
pub(crate) fn frame_size(sample_rate: i32) -> usize {
    match sample_rate {
        ..=48000 => 4096,
        48001..=96000 => 8192,
        _ => 16384,
    }
}

/// The averaged power spectrum of every non-silent frame, in dB, smoothed across about
/// 200 Hz so a single tone or a noisy bin cannot make or hide a cliff. None when too few
/// frames had sound in them. The second value is the number of active frames.
pub(crate) fn average_spectrum(windows: &[Vec<f32>], sample_rate: i32) -> (Option<Vec<f64>>, usize) {
    let size = frame_size(sample_rate);
    let hop = size / 2;
    let window = blackman_harris(size);
    let mut power = vec![0f64; size / 2 + 1];
    let mut re = vec![0f64; size];
    let mut im = vec![0f64; size];
    let silence = 10f64.powf(SILENCE_DB / 10.0);
    let mut active_frames = 0usize;

    for samples in windows {
        let mut start = 0;
        while start + size <= samples.len() {
            let mut energy = 0f64;
            for i in 0..size {
                let sample = samples[start + i];
                // float * float, as in C#, then widened into the double sum.
                energy += (sample * sample) as f64;
                re[i] = sample as f64 * window[i];
                im[i] = 0.0;
            }
            if energy / size as f64 >= silence {
                fft(&mut re, &mut im);
                for k in 0..power.len() {
                    power[k] += re[k] * re[k] + im[k] * im[k];
                }
                active_frames += 1;
            }
            start += hop;
        }
    }
    if active_frames < MIN_ACTIVE_FRAMES {
        return (None, active_frames);
    }

    let bin_hz = sample_rate as f64 / size as f64;
    let half = 1usize.max(net_format::round(100.0 / bin_hz) as usize);
    let mut db = vec![0f64; power.len()];
    for (k, level) in db.iter_mut().enumerate() {
        let from = k.saturating_sub(half);
        let to = (power.len() - 1).min(k + half);
        let sum: f64 = power[from..=to].iter().sum();
        *level = 10.0 * (sum / (to - from + 1) as f64 / active_frames as f64 + 1e-30).log10();
    }
    (Some(db), active_frames)
}

/// The verdict on decoded windows, separate from the process handling so it can be driven
/// directly.
pub(crate) fn judge(windows: &[Vec<f32>], sample_rate: i32) -> SpectrumReport {
    if sample_rate < 44100 {
        return SpectrumReport::unknown(
            format!("a sample rate of {sample_rate} Hz is under 44.1 kHz"),
            sample_rate,
        );
    }
    let Some(db) = average_spectrum(windows, sample_rate).0 else {
        return SpectrumReport::unknown("too little sound to judge", sample_rate);
    };

    let bin_hz = sample_rate as f64 / frame_size(sample_rate) as f64;
    let nyquist = sample_rate as f64 / 2.0;
    let last = db.len() as i64 - 1;
    let bins = |from_hz: f64, to_hz: f64| -> (usize, usize) {
        let from = (net_format::round(from_hz / bin_hz) as i64).clamp(0, last);
        let to = (net_format::round(to_hz / bin_hz) as i64).clamp(from, last);
        (from as usize, to as usize)
    };
    let mean = |from_hz: f64, to_hz: f64| -> f64 {
        let (from, to) = bins(from_hz, to_hz);
        db[from..=to].iter().sum::<f64>() / (to - from + 1) as f64
    };
    let max = |from_hz: f64, to_hz: f64| -> f64 {
        let (from, to) = bins(from_hz, to_hz);
        db[from..=to]
            .iter()
            .fold(f64::MIN, |max, &value| net_format::max(max, value))
    };

    let mid = mean(2000.0, 8000.0);

    // From the top down: the highest frequency where the half kilohertz below is far above
    // the half kilohertz above, and nothing higher comes back up.
    const GAP: f64 = 150.0;
    const REACH: f64 = 650.0;
    let top = nyquist - REACH - 50.0;
    let mut f = top;
    while f >= 10000.0 {
        let below = mean(f - REACH, f - GAP);
        let above = mean(f + GAP, f + REACH);
        if below - above < MIN_CLIFF_DB || max(f + GAP, nyquist - 100.0) - above > MAX_TAIL_RISE_DB {
            f -= bin_hz;
            continue;
        }

        // The cutoff is where the level crosses halfway down the cliff, the highest such
        // point, so a ragged shelf just below it does not pull the estimate down.
        let halfway = (below + above) / 2.0;
        let mut cutoff = f;
        let mut g = f + REACH;
        while g >= f - REACH {
            if mean(g, g) >= halfway {
                cutoff = g;
                break;
            }
            g -= bin_hz;
        }

        if cutoff >= GENUINE_CUTOFF_HZ {
            return SpectrumReport::new(
                SpectrumVerdict::Genuine,
                sample_rate,
                Some(net_format::round(cutoff)),
                format!("full band to {} kHz", net_format::fixed(cutoff / 1000.0, 1, 1)),
                None,
            );
        }
        // The whole depth, for the log: the scan stops at the first 30 dB it finds.
        let depth = mean(cutoff - 1200.0, cutoff - 400.0)
            - mean(cutoff + 400.0, (cutoff + 1200.0).min(nyquist - 100.0));
        return SpectrumReport::new(
            SpectrumVerdict::LikelyLossy,
            sample_rate,
            Some(net_format::round(cutoff)),
            format!(
                "a {} dB cliff at {} kHz",
                net_format::fixed(depth, 0, 0),
                net_format::fixed(cutoff / 1000.0, 1, 1)
            ),
            Some(estimate_for(cutoff).to_string()),
        );
    }

    // No cliff. Audio right up to the top of the CD band is genuine; a spectrum that fades
    // out before it may be a band-limited recording or a lossy file with a soft cutoff, and
    // that is no opinion.
    let high = mean(20000.0, 21000f64.min(nyquist - 200.0));
    if mid - high <= TOP_BAND_CONTENT_DB {
        return SpectrumReport::new(
            SpectrumVerdict::Genuine,
            sample_rate,
            None,
            "audio up to the top of the band",
            None,
        );
    }
    SpectrumReport::unknown("no clear cutoff", sample_rate)
}

/// The bitrate a cutoff is typical of, for a person reading a log. Encoders pick
/// their low-pass from the bitrate, so the cutoff gives the bitrate away roughly.
pub(crate) fn estimate_for(cutoff_hz: f64) -> &'static str {
    match cutoff_hz {
        c if c < 15500.0 => "a lossy file under 128 kbps",
        c if c < 17300.0 => "about 128 kbps MP3",
        c if c < 18200.0 => "about 160 kbps",
        c if c < 19400.0 => "about 192 kbps",
        _ => "about 256-320 kbps",
    }
}

/// The 4-term Blackman-Harris window. Its side lobes sit about 92 dB down, so the loud
/// midrange does not leak into the empty band above a cutoff and fill in the cliff.
pub(crate) fn blackman_harris(size: usize) -> Vec<f64> {
    (0..size)
        .map(|i| {
            let x = 2.0 * std::f64::consts::PI * i as f64 / (size as f64 - 1.0);
            0.35875 - 0.48829 * x.cos() + 0.14128 * (2.0 * x).cos() - 0.01168 * (3.0 * x).cos()
        })
        .collect()
}

/// In-place iterative radix-2 FFT. The length must be a power of two; anything else panics,
/// as C# threw an `ArgumentException`.
pub(crate) fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert!(n != 0 && n.is_power_of_two(), "the length must be a power of two");

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut length = 2;
    while length <= n {
        let angle = -2.0 * std::f64::consts::PI / length as f64;
        let step_re = angle.cos();
        let step_im = angle.sin();
        let mut i = 0;
        while i < n {
            let (mut w_re, mut w_im) = (1f64, 0f64);
            for k in 0..length / 2 {
                let a = i + k;
                let b = a + length / 2;
                let t_re = re[b] * w_re - im[b] * w_im;
                let t_im = re[b] * w_im + im[b] * w_re;
                re[b] = re[a] - t_re;
                im[b] = im[a] - t_im;
                re[a] += t_re;
                im[a] += t_im;
                let next = w_re * step_re - w_im * step_im;
                w_im = w_re * step_im + w_im * step_re;
                w_re = next;
            }
            i += length;
        }
        length <<= 1;
    }
}

#[cfg(test)]
#[path = "spectrum_analyzer_tests.rs"]
mod tests;
