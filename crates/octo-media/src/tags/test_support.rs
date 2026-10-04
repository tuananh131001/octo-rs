//! Shared pieces of the tag tests: the C# tests' synthetic audio (`AudioFixtures`) and the
//! checked-in fixtures under docs/rust-migration/fixtures/tags.

use std::path::{Path, PathBuf};

/// docs/rust-migration/fixtures/tags.
pub(crate) fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rust-migration/fixtures/tags")
}

/// `AudioFixtures.Mp3`: twenty silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz.
pub(crate) fn mp3() -> Vec<u8> {
    const FRAME_LENGTH: usize = 417;
    let mut bytes = vec![0u8; FRAME_LENGTH * 20];
    for frame in 0..20 {
        let offset = frame * FRAME_LENGTH;
        bytes[offset..offset + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    bytes
}

/// `AudioFixtures.Flac`: a FLAC with a STREAMINFO block describing two seconds of 16-bit stereo
/// and no frames.
pub(crate) fn flac() -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let (sample_rate, channels_minus_one, bits_minus_one, total_samples) = (44100u64, 1u64, 15u64, 88200u64);
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

/// A temporary directory with a file of the given bytes in it, named `name`.
pub(crate) fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("written");
    path
}
