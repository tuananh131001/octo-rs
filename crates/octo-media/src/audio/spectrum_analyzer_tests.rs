//! The fake-lossless check. A wrong LikelyLossy makes Octo throw away a good download and go
//! looking for another, so the cases that must NOT be called fake matter as much as the ones
//! that must.
//!
//! Real audio, made by ffmpeg when the tests run: stereo pink noise as the music, the same noise
//! through an MP3 encoder and back to FLAC as the fake, and the cases that must never be called
//! fake. Each file is twelve seconds. The C# fixture made them all once per class; here each
//! test makes the files it needs in its own temp folder, which cleans itself up.

use std::path::Path;

use super::*;
use crate::audio::test_support::{require_ffmpeg, run_ffmpeg};

/// Two noise sources with different seeds, so the stereo is real stereo: an encoder given
/// identical channels spends its bits on one and cuts far higher than it does for music.
const NOISE: &str = "anoisesrc=color=pink:duration=12:sample_rate=44100:amplitude=0.3";

/// Makes `name` (and what it is made from) in `dir`, with the fixture's exact commands.
fn make(dir: &Path, name: &str) {
    if dir.join(name).exists() {
        return;
    }
    match name {
        "genuine.flac" => run_ffmpeg(
            dir,
            &format!(
                "-filter_complex {NOISE}:seed=1[a];{NOISE}:seed=2[b];[a][b]amerge=inputs=2 -c:a flac -sample_fmt s16 genuine.flac"
            ),
        ),
        "mp3_128.flac" | "mp3_320.flac" => {
            let bitrate = &name[4..7];
            make(dir, "genuine.flac");
            run_ffmpeg(
                dir,
                &format!("-i genuine.flac -c:a libmp3lame -b:a {bitrate}k mp3_{bitrate}.mp3"),
            );
            run_ffmpeg(
                dir,
                &format!("-i mp3_{bitrate}.mp3 -c:a flac -sample_fmt s16 mp3_{bitrate}.flac"),
            );
        }
        // A genuinely band-limited recording: four poles at 5 kHz, 24 dB an octave, so there is
        // almost nothing above 15 kHz and no cliff anywhere.
        "gentle.flac" => {
            make(dir, "genuine.flac");
            run_ffmpeg(
                dir,
                "-i genuine.flac -af lowpass=f=5000,lowpass=f=5000 -c:a flac -sample_fmt s16 gentle.flac",
            );
        }
        "silence.flac" => run_ffmpeg(
            dir,
            "-f lavfi -i anullsrc=r=44100:cl=stereo -t 12 -c:a flac -sample_fmt s16 silence.flac",
        ),
        "low_rate.flac" => run_ffmpeg(
            dir,
            "-f lavfi -i anoisesrc=color=pink:duration=12:sample_rate=22050:amplitude=0.3 -c:a flac -sample_fmt s16 low_rate.flac",
        ),
        _ => panic!("no recipe for {name}"),
    }
}

async fn analyze(name: &str) -> SpectrumReport {
    let dir = tempfile::tempdir().expect("a temp dir");
    make(dir.path(), name);
    let report = SpectrumAnalyzer::new().analyze(&dir.path().join(name), 30).await;
    println!(
        "{name}: {:?}, cutoff {} Hz, {}",
        report.verdict,
        report
            .cutoff_hz
            .map(|hz| hz.to_string())
            .unwrap_or_else(|| "none".into()),
        report.reason
    );
    report
}

#[tokio::test]
async fn full_band_lossless_is_genuine() {
    require_ffmpeg!();
    let report = analyze("genuine.flac").await;
    assert_eq!(report.verdict, SpectrumVerdict::Genuine, "{report:?}");
}

#[tokio::test]
async fn a128k_mp3_made_into_flac_is_likely_lossy() {
    require_ffmpeg!();
    let report = analyze("mp3_128.flac").await;
    assert_eq!(report.verdict, SpectrumVerdict::LikelyLossy, "{report:?}");
    // The encoder's own low-pass for 128 kbps stereo is 17 kHz.
    let cutoff = report.cutoff_hz.expect("a cutoff");
    assert!((16000.0..=17300.0).contains(&cutoff), "cutoff {cutoff}");
    assert_eq!(report.estimate.as_deref(), Some("about 128 kbps MP3"));
}

#[tokio::test]
async fn a320k_mp3_made_into_flac_is_likely_lossy() {
    require_ffmpeg!();
    let report = analyze("mp3_320.flac").await;
    assert_eq!(report.verdict, SpectrumVerdict::LikelyLossy, "{report:?}");
    let cutoff = report.cutoff_hz.expect("a cutoff");
    assert!((19600.0..=GENUINE_CUTOFF_HZ).contains(&cutoff), "cutoff {cutoff}");
    assert_eq!(report.estimate.as_deref(), Some("about 256-320 kbps"));
}

/// The false positive this is built to avoid: little treble is not a cutoff.
#[tokio::test]
async fn a_gentle_roll_off_is_never_called_lossy() {
    require_ffmpeg!();
    let report = analyze("gentle.flac").await;
    assert_ne!(report.verdict, SpectrumVerdict::LikelyLossy, "{report:?}");
}

#[tokio::test]
async fn silence_is_unknown() {
    require_ffmpeg!();
    let report = analyze("silence.flac").await;
    assert_eq!(report.verdict, SpectrumVerdict::Unknown, "{report:?}");
}

#[tokio::test]
async fn a_sample_rate_under44k_is_unknown() {
    require_ffmpeg!();
    let report = analyze("low_rate.flac").await;
    assert_eq!(report.verdict, SpectrumVerdict::Unknown, "{report:?}");
    assert_eq!(report.sample_rate, 22050);
}

#[tokio::test]
async fn a_file_that_is_not_there_is_no_opinion_not_an_exception() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let report = SpectrumAnalyzer::new()
        .analyze(&dir.path().join("missing.flac"), 5)
        .await;
    assert_eq!(report.verdict, SpectrumVerdict::Unknown);
}

#[test]
fn only_a_lossless_extension_makes_the_claim() {
    let cases = [
        ("song.flac", true),
        ("SONG.FLAC", true),
        ("song.wav", true),
        ("song.aiff", true),
        ("song.ape", true),
        ("song.mp3", false),
        ("song.m4a", false),
        ("", false),
    ];
    for (path, claims) in cases {
        assert_eq!(SpectrumAnalyzer::claims_lossless(path), claims, "path {path:?}");
    }
}

#[test]
fn the_cutoff_gives_the_bitrate_away() {
    let cases = [
        (14000.0, "a lossy file under 128 kbps"),
        (16000.0, "about 128 kbps MP3"),
        (17000.0, "about 128 kbps MP3"),
        (17600.0, "about 160 kbps"),
        (19000.0, "about 192 kbps"),
        (19700.0, "about 256-320 kbps"),
        (20400.0, "about 256-320 kbps"),
    ];
    for (cutoff, estimate) in cases {
        assert_eq!(estimate_for(cutoff), estimate, "cutoff {cutoff}");
    }
}

#[test]
fn windows_are_spread_through_the_track_and_stay_inside_it() {
    let starts = window_starts(200.0);
    assert_eq!(starts.len(), WINDOWS);
    for start in &starts {
        assert!((0.0..=200.0 - WINDOW_SECONDS).contains(start), "start {start}");
    }
    assert_eq!(window_starts(4.0), vec![0.0]);
}

#[test]
fn the_fft_puts_a_tones_energy_in_its_bin() {
    const SIZE: usize = 1024;
    let mut re: Vec<f64> = (0..SIZE)
        .map(|i| (2.0 * std::f64::consts::PI * 37.0 * i as f64 / SIZE as f64).sin())
        .collect();
    let mut im = vec![0f64; SIZE];

    fft(&mut re, &mut im);

    let magnitude: Vec<f64> = (0..SIZE / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
        .collect();
    let peak = magnitude
        .iter()
        .enumerate()
        .fold(
            (0, f64::MIN),
            |best, (k, &value)| if value > best.1 { (k, value) } else { best },
        )
        .0;
    assert_eq!(peak, 37);
    assert_eq!(
        net_format::round_digits(magnitude[37], 6),
        net_format::round_digits(SIZE as f64 / 2.0, 6)
    );
    assert!(
        magnitude
            .iter()
            .enumerate()
            .filter(|&(k, _)| k != 37)
            .all(|(_, &value)| value < 1e-6)
    );
}

#[test]
#[should_panic(expected = "the length must be a power of two")]
fn a_frame_that_is_not_a_power_of_two_is_refused() {
    fft(&mut [0f64; 1000], &mut [0f64; 1000]);
}

/// Synthetic samples straight into the judge: a spectrum with nothing in it is not a
/// verdict about the file.
#[test]
fn too_little_sound_is_unknown() {
    let quiet = vec![0f32; 44100 * 5];
    assert_eq!(judge(&[quiet], 44100).verdict, SpectrumVerdict::Unknown);
}

// ---- From TranscodeDecisionTests: the parts that are SpectrumReport's own ----------------

/// `LibraryActionExecutor.NotReallyLossless` is `"is " + Describe()` for a likely transcode.
#[test]
fn a_likely_transcode_describes_its_source_and_cutoff() {
    let fake = SpectrumReport::new(
        SpectrumVerdict::LikelyLossy,
        44100,
        Some(16929.0),
        "a cliff",
        Some("about 128 kbps MP3".into()),
    );
    assert!(fake.is_likely_lossy());
    assert_eq!(
        fake.describe(),
        "likely transcoded from about 128 kbps MP3 (cutoff 16.9 kHz)"
    );
}

#[test]
fn unknown_and_genuine_are_not_likely_lossy() {
    let unknown = SpectrumReport::unknown("not checked", 0);
    assert!(!unknown.is_likely_lossy());
    assert_eq!(unknown.describe(), "no opinion (not checked)");
    let genuine = SpectrumReport::new(
        SpectrumVerdict::Genuine,
        44100,
        None,
        "audio up to the top of the band",
        None,
    );
    assert!(!genuine.is_likely_lossy());
    assert_eq!(genuine.describe(), "genuine (audio up to the top of the band)");
}
