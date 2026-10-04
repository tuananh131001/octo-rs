//! What an `HttpClient` call gave the C# callers: by default (`ResponseContentRead`) the whole
//! body was read before `SendAsync` returned, inside the client's timeout. [`HttpAnswer`] is that
//! buffered response, and the helpers here read it the way `ReadAsStringAsync` and
//! `JsonDocument.Parse` did.

use bytes::Bytes;
use reqwest::StatusCode;
use serde_json::Value;

/// A response with its body read.
#[derive(Debug, Clone)]
pub struct HttpAnswer {
    pub status: StatusCode,
    pub body: Bytes,
}

impl HttpAnswer {
    /// Reads the whole body of a response.
    pub async fn read(response: reqwest::Response) -> reqwest::Result<HttpAnswer> {
        let status = response.status();
        let body = response.bytes().await?;
        Ok(HttpAnswer { status, body })
    }

    /// An answer the client made up itself, with no body (the limiters' 429).
    pub fn empty(status: StatusCode) -> HttpAnswer {
        HttpAnswer {
            status,
            body: Bytes::new(),
        }
    }

    /// `IsSuccessStatusCode`: 200-299.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// `ReadAsStringAsync`: the body as UTF-8, a byte order mark dropped, bad bytes replaced.
    pub fn text(&self) -> String {
        let body = self.body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&self.body);
        String::from_utf8_lossy(body).into_owned()
    }

    /// `JsonDocument.Parse` of the body.
    pub fn json(&self) -> serde_json::Result<Value> {
        parse_json(&self.body)
    }
}

/// `JsonDocument.Parse` over bytes: a UTF-8 byte order mark is skipped.
pub fn parse_json(bytes: &[u8]) -> serde_json::Result<Value> {
    serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
}

/// A client builder with `SocketsHttpHandler`'s defaults where reqwest's differ: no automatic
/// decompression (so no Accept-Encoding header), which the C# only turned on for AcoustID.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_gzip().no_deflate()
}

/// `new Uri(baseAddress, relative)`, as `HttpClient.BaseAddress` resolved a relative request.
pub fn resolve(base: &url::Url, relative: &str) -> Result<url::Url, url::ParseError> {
    base.join(relative)
}
