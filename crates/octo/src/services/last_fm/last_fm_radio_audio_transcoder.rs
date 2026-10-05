//! Port of `Services/LastFm/LastFmRadioAudioTranscoder.cs`: ffmpeg turns each radio track into
//! a headerless MP3 segment at the listener's loudness target. The profile record, the gain and
//! the spectral reading are `octo_core::last_fm::last_fm_radio_audio_transcoder`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use futures::StreamExt;
use octo_core::last_fm::RadioAudioProfile;
use octo_core::last_fm::last_fm_radio_audio_transcoder::{
    LIMITER_CEILING, gain_for, gain_text, read_spectral,
};
use octo_media::audio::LoudnessMeter;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::OperationCanceled;
use crate::services::i_download_service::AudioStream;

#[async_trait]
pub trait ILastFmRadioAudioTranscoder: Send + Sync {
    /// Transcodes one track to a headerless MP3 segment on `output`. When `target_lufs` is set
    /// the track is first measured (EBU R128) and brought to that integrated loudness with a
    /// static gain and a true-peak limiter. Returns what was measured, or None when measurement
    /// was not possible.
    async fn transcode_to_mp3(
        &self,
        input: AudioStream,
        output: &mut (dyn AsyncWrite + Unpin + Send),
        bitrate_kbps: i32,
        target_lufs: Option<f64>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<RadioAudioProfile>>;
}

/// Normalizes mixed local FLAC and external M4A sources into one MP3 byte stream. A fresh
/// process per song prevents decoder state leaking across track/container boundaries; its stdout
/// is appended to the same client response. The input is spooled to a temporary file so it can
/// be read twice: once to measure, once to encode with the measured gain.
#[derive(Debug, Default)]
pub struct FfmpegLastFmRadioAudioTranscoder;

#[async_trait]
impl ILastFmRadioAudioTranscoder for FfmpegLastFmRadioAudioTranscoder {
    async fn transcode_to_mp3(
        &self,
        input: AudioStream,
        output: &mut (dyn AsyncWrite + Unpin + Send),
        bitrate_kbps: i32,
        target_lufs: Option<f64>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<RadioAudioProfile>> {
        let spool = std::env::temp_dir().join(format!("octo-radio-in-{}", uuid::Uuid::new_v4().simple()));
        let spectral = with_suffix(&spool, ".spectral");
        let result = transcode(
            &spool,
            &spectral,
            input,
            output,
            bitrate_kbps,
            target_lufs,
            cancellation_token,
        )
        .await;
        // Temp cleanup is best effort.
        let _ = std::fs::remove_file(&spool);
        let _ = std::fs::remove_file(&spectral);
        result
    }
}

async fn transcode(
    spool: &Path,
    spectral: &Path,
    mut input: AudioStream,
    output: &mut (dyn AsyncWrite + Unpin + Send),
    bitrate_kbps: i32,
    target_lufs: Option<f64>,
    cancellation_token: &CancellationToken,
) -> anyhow::Result<Option<RadioAudioProfile>> {
    {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(spool)
            .await?;
        loop {
            let chunk = tokio::select! {
                biased;
                () = cancellation_token.cancelled() => return Err(OperationCanceled.into()),
                chunk = input.next() => chunk,
            };
            match chunk {
                Some(bytes) => file.write_all(&bytes?).await?,
                None => break,
            }
        }
        file.flush().await?;
    }

    let measured = measure_loudness(spool, cancellation_token).await?;
    let gain = gain_for(measured.map(|m| m.0), target_lufs);

    let mut filters = Vec::new();
    if gain != 0.0 {
        filters.push(format!("volume={}dB", gain_text(gain)));
        filters.push(format!(
            "alimiter=limit={LIMITER_CEILING}:level=false:attack=5:release=50"
        ));
    }
    // The metadata sink is named relative to the temp directory, which `run` uses as the
    // working directory: a filter option value cannot carry the drive colon or backslashes of an
    // absolute Windows path unescaped.
    filters.push("aspectralstats=win_size=4096".to_string());
    filters.push(format!(
        "ametadata=mode=print:file={}",
        spectral.file_name().unwrap_or_default().to_string_lossy()
    ));

    let spool_text = spool.to_string_lossy();
    let filter_text = filters.join(",");
    let bitrate = format!("{bitrate_kbps}k");
    run(
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &spool_text,
            "-vn",
            "-map_metadata",
            "-1",
            "-af",
            &filter_text,
            "-codec:a",
            "libmp3lame",
            "-b:a",
            &bitrate,
            "-write_xing",
            "0",
            "-id3v2_version",
            "0",
            "-f",
            "mp3",
            "pipe:1",
        ],
        output,
        cancellation_token,
        false,
    )
    .await?;

    let Some((integrated, range, peak)) = measured else {
        return Ok(None);
    };
    let (centroid, flatness, rolloff) = match std::fs::read(spectral) {
        Ok(bytes) => read_spectral(&String::from_utf8_lossy(&bytes))?,
        Err(_) => (0.0, 0.0, 0.0),
    };
    Ok(Some(RadioAudioProfile::new(
        integrated, range, peak, gain, centroid, flatness, rolloff,
    )))
}

/// The integrated loudness, loudness range and true peak, or None when ffmpeg could not measure
/// it. A summary level that is not a number fails the transcode, as the C# `double.Parse` threw.
async fn measure_loudness(
    spool: &Path,
    cancellation_token: &CancellationToken,
) -> anyhow::Result<Option<(f64, f64, f64)>> {
    // ebur128 prints its summary on stderr at the default log level; nothing else in this
    // invocation writes there. Decode to a fixed format first because a stream whose first
    // packet probes differently from the rest re-initialises the graph and the scanner with it.
    let spool_text = spool.to_string_lossy();
    let mut sink = tokio::io::sink();
    let report = match run(
        &[
            "-hide_banner",
            "-nostats",
            "-i",
            &spool_text,
            "-af",
            "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo,ebur128=peak=true",
            "-f",
            "null",
            "-",
        ],
        &mut sink,
        cancellation_token,
        true,
    )
    .await
    {
        Ok(report) => report,
        Err(error) if error.is::<OperationCanceled>() => return Err(error),
        Err(_) => return Ok(None),
    };

    // The same summary the download path reads for ReplayGain, parsed in one place.
    Ok(LoudnessMeter::parse(&report)?.map(|m| (m.integrated_lufs, m.loudness_range_lu, m.true_peak_dbfs)))
}

/// Runs ffmpeg with stdout copied to `output`; returns stderr.
async fn run(
    arguments: &[&str],
    output: &mut (dyn AsyncWrite + Unpin + Send),
    cancellation_token: &CancellationToken,
    tolerate_failure: bool,
) -> anyhow::Result<String> {
    let mut child = Command::new("ffmpeg")
        .args(arguments)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            anyhow::Error::new(error).context("Continuous Radio needs ffmpeg in the Octo runtime image")
        })?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let work = async {
        let copy = copy_all(&mut stdout, output);
        let errors = async {
            let mut text = Vec::new();
            stderr.read_to_end(&mut text).await.map(|_| text)
        };
        let (copied, errors) = tokio::join!(copy, errors);
        copied?;
        let errors = String::from_utf8_lossy(&errors?).into_owned();
        let status = child.wait().await?;
        anyhow::Ok((status, errors))
    };
    let (status, errors) = tokio::select! {
        biased;
        () = cancellation_token.cancelled() => {
            // Dropping the work drops the child, which kills it (best effort during
            // disconnect/shutdown).
            return Err(OperationCanceled.into());
        }
        result = work => result?,
    };
    if !status.success() && !tolerate_failure {
        anyhow::bail!("ffmpeg exited {}: {}", status.code().unwrap_or(-1), errors.trim());
    }
    Ok(errors)
}

async fn copy_all(
    from: &mut (impl AsyncRead + Unpin),
    to: &mut (dyn AsyncWrite + Unpin + Send),
) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 81920];
    loop {
        let read = from.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        to.write_all(&buffer[..read]).await?;
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_owned();
    text.push(suffix);
    PathBuf::from(text)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    macro_rules! require_ffmpeg {
        () => {
            if !ffmpeg_available() {
                eprintln!("skipped: ffmpeg is not on the PATH");
                return;
            }
        };
    }

    fn wav_tone(seconds: f64, amplitude: f64, frequency_hz: f64) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44100;
        let samples = (f64::from(SAMPLE_RATE) * seconds) as u32;
        let data_length = samples * 2;
        let mut bytes = Vec::with_capacity(44 + data_length as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_length).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_length.to_le_bytes());
        for index in 0..samples {
            let value = amplitude
                * f64::from(i16::MAX)
                * (2.0 * std::f64::consts::PI * frequency_hz * f64::from(index) / f64::from(SAMPLE_RATE))
                    .sin();
            bytes.extend_from_slice(&(value as i16).to_le_bytes());
        }
        bytes
    }

    fn stream_of(bytes: Vec<u8>) -> AudioStream {
        Box::pin(futures::stream::iter([Ok(Bytes::from(bytes))]))
    }

    fn wav_silence() -> AudioStream {
        stream_of(wav_tone(0.25, 0.0, 440.0))
    }

    // LastFmRadioCoreTests.FfmpegTranscoder_ProducesConcatenableHeaderlessMp3Segments
    #[tokio::test]
    async fn ffmpeg_transcoder_produces_concatenable_headerless_mp3_segments() {
        require_ffmpeg!();
        let transcoder = FfmpegLastFmRadioAudioTranscoder;
        let none = CancellationToken::new();
        let mut output: Vec<u8> = Vec::new();
        transcoder
            .transcode_to_mp3(wav_silence(), &mut output, 192, Some(-16.0), &none)
            .await
            .unwrap();
        let boundary = output.len();
        transcoder
            .transcode_to_mp3(wav_silence(), &mut output, 192, None, &none)
            .await
            .unwrap();
        assert!(boundary > 100);
        assert!(output.len() > boundary + 100);
        assert_ne!(&output[..3], b"ID3");
        assert_ne!(&output[boundary..boundary + 3], b"ID3");
        assert_eq!(output[0], 0xff);
        assert_eq!(output[1] & 0xe0, 0xe0);
        assert_eq!(output[boundary], 0xff);
        assert_eq!(output[boundary + 1] & 0xe0, 0xe0);
    }

    // LastFmRadioCoreTests.FfmpegTranscoder_BringsATrackToTheLoudnessTargetAndReportsItsProfile
    #[tokio::test]
    async fn ffmpeg_transcoder_brings_a_track_to_the_loudness_target_and_reports_its_profile() {
        require_ffmpeg!();
        // A -6 dBFS sine sits far above -16 LUFS; a 1 kHz tone puts the centroid at 1 kHz.
        let transcoder = FfmpegLastFmRadioAudioTranscoder;
        let none = CancellationToken::new();
        let mut output: Vec<u8> = Vec::new();
        let profile = transcoder
            .transcode_to_mp3(
                stream_of(wav_tone(4.0, 0.5, 1000.0)),
                &mut output,
                192,
                Some(-16.0),
                &none,
            )
            .await
            .unwrap()
            .expect("a profile");

        assert!(
            profile.integrated_lufs > -12.0,
            "source measured {} LUFS",
            profile.integrated_lufs
        );
        assert!((-12.0..=-2.0).contains(&profile.gain_db), "{}", profile.gain_db);
        assert!(
            (700.0..=1400.0).contains(&profile.spectral_centroid_hz),
            "{}",
            profile.spectral_centroid_hz
        );

        // Re-measure the encoded output: it should now sit at the target.
        let mut reencoded: Vec<u8> = Vec::new();
        let again = transcoder
            .transcode_to_mp3(stream_of(output), &mut reencoded, 192, None, &none)
            .await
            .unwrap()
            .expect("a profile");
        assert!(
            (-17.5..=-14.5).contains(&again.integrated_lufs),
            "{}",
            again.integrated_lufs
        );
        assert_eq!(again.gain_db, 0.0);
    }

    /// Rust-only: input that is not audio fails the encode with ffmpeg's own words, and leaves
    /// no spool behind.
    #[tokio::test]
    async fn input_that_is_not_audio_fails_with_ffmpegs_words() {
        require_ffmpeg!();
        let mut output: Vec<u8> = Vec::new();
        let error = FfmpegLastFmRadioAudioTranscoder
            .transcode_to_mp3(
                stream_of(b"not audio at all".to_vec()),
                &mut output,
                192,
                Some(-16.0),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().starts_with("ffmpeg exited"), "{error}");
    }
}
