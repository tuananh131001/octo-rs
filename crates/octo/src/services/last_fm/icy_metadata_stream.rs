//! Port of `Services/LastFm/IcyMetadataStream.cs`.
//!
//! Adds ICY framing around Octo's existing station-track metadata. This stream owns no discovery
//! or artwork logic; it only transports the artist/title that the radio snapshot already
//! selected.

use octo_core::common::dotnet;
use octo_core::models::radio::LastFmRadioTrack;
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub const DEFAULT_INTERVAL: usize = 16 * 1024;
const MAXIMUM_METADATA_BYTES: usize = u8::MAX as usize * 16;

/// A write-only wrapper: every `interval` audio bytes it writes the current metadata block.
pub struct IcyMetadataStream<W> {
    inner: W,
    interval: usize,
    audio_bytes_until_metadata: usize,
    metadata_block: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> IcyMetadataStream<W> {
    /// `interval` must be positive (C# threw `ArgumentOutOfRangeException`).
    pub fn new(inner: W, interval: usize) -> Self {
        assert!(interval > 0, "the ICY interval must be positive");
        IcyMetadataStream {
            inner,
            interval,
            audio_bytes_until_metadata: interval,
            metadata_block: vec![0],
        }
    }

    pub fn set_track(&mut self, track: &LastFmRadioTrack) {
        self.metadata_block = encode(&track.artist, &track.title);
    }

    /// Writes audio, with the metadata block after every `interval` bytes of it.
    pub async fn write(&mut self, mut buffer: &[u8]) -> std::io::Result<()> {
        while !buffer.is_empty() {
            let count = buffer.len().min(self.audio_bytes_until_metadata);
            self.inner.write_all(&buffer[..count]).await?;
            buffer = &buffer[count..];
            self.audio_bytes_until_metadata -= count;
            if self.audio_bytes_until_metadata != 0 {
                continue;
            }

            self.inner.write_all(&self.metadata_block).await?;
            self.audio_bytes_until_metadata = self.interval;
        }
        Ok(())
    }

    pub async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// One metadata block: a length byte (in 16-byte units), then `StreamTitle='Artist - Title';`
/// in UTF-8, zero-padded. Control characters are dropped and an apostrophe becomes ’ so the
/// value cannot close its own quotes; a title too long for 4080 bytes is cut on a character
/// boundary.
pub fn encode(artist: &str, title: &str) -> Vec<u8> {
    let stream_title = [artist, title]
        .into_iter()
        .filter(|value| !dotnet::is_blank(value))
        .collect::<Vec<_>>()
        .join(" - ");
    let stream_title: String = stream_title
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| if character == '\'' { '’' } else { character })
        .collect();
    let payload = format!("StreamTitle='{stream_title}';").into_bytes();
    let mut payload_length = payload.len().min(MAXIMUM_METADATA_BYTES);
    while payload_length > 0 && payload_length < payload.len() && (payload[payload_length] & 0xc0) == 0x80 {
        payload_length -= 1;
    }

    let block_count = payload_length.div_ceil(16);
    let mut block = vec![0u8; 1 + block_count * 16];
    block[0] = u8::try_from(block_count).expect("at most 255 blocks");
    block[1..1 + payload_length].copy_from_slice(&payload[..payload_length]);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_audio(bytes: &[u8], offset: &mut usize) -> String {
        let value = String::from_utf8_lossy(&bytes[*offset..*offset + 4]).into_owned();
        *offset += 4;
        value
    }

    fn read_metadata(bytes: &[u8], offset: &mut usize) -> String {
        let length = bytes[*offset] as usize * 16;
        *offset += 1;
        let value = String::from_utf8_lossy(&bytes[*offset..*offset + length])
            .trim_end_matches('\0')
            .to_string();
        *offset += length;
        value
    }

    // LastFmRadioCoreTests.IcyMetadataStream_FramesExistingTrackMetadataAtFixedAudioIntervals
    #[tokio::test]
    async fn icy_metadata_stream_frames_existing_track_metadata_at_fixed_audio_intervals() {
        let mut stream = IcyMetadataStream::new(Vec::new(), 4);
        stream.set_track(&LastFmRadioTrack {
            artist: "Artist One".into(),
            title: "Song One".into(),
            ..Default::default()
        });
        stream.write(b"ABCDEFGH").await.unwrap();
        stream.set_track(&LastFmRadioTrack {
            artist: "Artist Two".into(),
            title: "Song Two".into(),
            ..Default::default()
        });
        stream.write(b"IJKL").await.unwrap();

        let bytes = stream.into_inner();
        let mut offset = 0;
        assert_eq!(read_audio(&bytes, &mut offset), "ABCD");
        assert_eq!(
            read_metadata(&bytes, &mut offset),
            "StreamTitle='Artist One - Song One';"
        );
        assert_eq!(read_audio(&bytes, &mut offset), "EFGH");
        assert_eq!(
            read_metadata(&bytes, &mut offset),
            "StreamTitle='Artist One - Song One';"
        );
        assert_eq!(read_audio(&bytes, &mut offset), "IJKL");
        assert_eq!(
            read_metadata(&bytes, &mut offset),
            "StreamTitle='Artist Two - Song Two';"
        );
        assert_eq!(bytes.len(), offset);
    }

    /// Rust-only: no track is an empty block, quotes and control characters are made safe, and a
    /// long title is cut on a character boundary.
    #[test]
    fn blocks_are_safe_and_bounded() {
        assert_eq!(encode("", " "), {
            let mut block = vec![1u8];
            block.extend_from_slice(b"StreamTitle='';");
            block.push(0);
            block
        });
        let block = encode("Guns N' Roses", "Line\u{7}Break");
        let text = String::from_utf8_lossy(&block[1..])
            .trim_end_matches('\0')
            .to_string();
        assert_eq!(text, "StreamTitle='Guns N’ Roses - LineBreak';");
        let long = encode("é".repeat(3000).as_str(), "x");
        assert_eq!(long[0], 255);
        assert_eq!(long.len(), 1 + 255 * 16);
        let payload = &long[1..];
        let end = payload.iter().rposition(|b| *b != 0).unwrap() + 1;
        assert!(std::str::from_utf8(&payload[..end]).is_ok());
    }
}
