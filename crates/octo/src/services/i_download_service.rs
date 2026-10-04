//! STUB(4-B): replaced when 4-B (download base) lands with the port of
//! `Services/IDownloadService.cs`. Only `DirectStreamInfo` is here, which
//! `SubsonicProxyService::open_audio_stream` (3-E) returns.

use std::pin::Pin;

use bytes::Bytes;
use futures::Stream;

/// An audio body being read, chunk by chunk (the C# `Stream`). Dropping it closes the upstream
/// response it reads from.
pub type AudioStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Information about a direct stream from a provider CDN.
pub struct DirectStreamInfo {
    pub audio_stream: AudioStream,
    pub content_type: String,
    pub content_length: Option<u64>,
    pub quality: Option<String>,
    /// 200 for full body, 206 for partial content.
    pub status_code: u16,
    /// Verbatim Content-Range header to forward to the client (only set when StatusCode == 206).
    pub content_range: Option<String>,
}
