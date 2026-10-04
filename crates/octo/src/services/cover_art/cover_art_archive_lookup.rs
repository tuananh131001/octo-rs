//! Port of `Services/CoverArt/CoverArtArchiveLookup.cs`, with its named client (`Program.cs`:
//! base address https://coverartarchive.org/, an 8 s timeout, Octo's User-Agent).

use std::time::Duration;

use bytes::Bytes;
use octo_core::common::octo_user_agent;
use tracing::debug;
use url::Url;

use crate::services::framework::HttpAnswer;
use crate::services::framework::http::{client_builder, resolve};

/// The Cover Art Archive, asked directly when a fingerprint named the MusicBrainz release: the
/// right pressing, no guessing by name (#51). The front of the release first, then of the release
/// group. No key; it redirects across hosts to archive.org, which the client follows. Asked at
/// 1200, its largest thumbnail: the 500 one lost to the catalog's 1000 px covers.
pub struct CoverArtArchiveLookup {
    client: reqwest::Client,
    base: Url,
}

impl CoverArtArchiveLookup {
    pub const CLIENT_NAME: &'static str = "coverartarchive";
    pub const BASE_ADDRESS: &'static str = "https://coverartarchive.org/";

    pub fn new() -> Self {
        Self::with_base_url(Url::parse(Self::BASE_ADDRESS).expect("the Cover Art Archive address parses"))
    }

    /// A lookup against another base address: a test's mock server.
    pub fn with_base_url(base: Url) -> Self {
        // Short timeout because the whole finalize phase runs under the download lock.
        let client = client_builder()
            .timeout(Duration::from_secs(8))
            .user_agent(octo_user_agent::value())
            .build()
            .expect("the Cover Art Archive client builds");
        Self { client, base }
    }

    pub async fn try_fetch(&self, release_id: Option<&str>, release_group_id: Option<&str>) -> Option<Bytes> {
        let release_id = release_id.filter(|id| !id.is_empty());
        let release_group_id = release_group_id.filter(|id| !id.is_empty());
        // The 500 one only when the 1200 one is missing, which an old upload can be.
        let paths = [
            release_id.map(|id| format!("release/{id}/front-1200")),
            release_id.map(|id| format!("release/{id}/front-500")),
            release_group_id.map(|id| format!("release-group/{id}/front-1200")),
            release_group_id.map(|id| format!("release-group/{id}/front-500")),
        ];
        for path in paths.into_iter().flatten() {
            let attempt = async {
                let url = resolve(&self.base, &path)?;
                anyhow::Ok(HttpAnswer::read(self.client.get(url).send().await?).await?)
            };
            match attempt.await {
                // 404 is the ordinary answer for a release nobody has uploaded art for.
                Ok(answer) if answer.is_success() => return Some(answer.body),
                Ok(_) => {}
                Err(e) => debug!("cover art archive {path} failed: {e}"),
            }
        }
        None
    }
}

impl Default for CoverArtArchiveLookup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Rust-only: the release's 1200 first, then its 500, then the group's; a redirect to
    /// another host is followed.
    #[tokio::test]
    async fn the_release_comes_before_its_group_and_redirects_are_followed() {
        let server = MockServer::start().await;
        let elsewhere = format!("http://localhost:{}/archive/front.jpg", server.address().port());
        Mock::given(path("/release-group/g1/front-1200"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", elsewhere.as_str()))
            .mount(&server)
            .await;
        Mock::given(path("/archive/front.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"group-front".to_vec()))
            .mount(&server)
            .await;
        Mock::given(path("/release/r2/front-500"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"release-500".to_vec()))
            .mount(&server)
            .await;
        let base = Url::parse(&format!("{}/", server.uri())).expect("parses");
        let lookup = CoverArtArchiveLookup::with_base_url(base);

        assert_eq!(
            lookup.try_fetch(Some("r1"), Some("g1")).await.as_deref(),
            Some(&b"group-front"[..])
        );
        assert_eq!(
            lookup.try_fetch(Some("r2"), Some("g1")).await.as_deref(),
            Some(&b"release-500"[..])
        );
        assert_eq!(lookup.try_fetch(Some(""), None).await, None);
        let requests = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            requests[0]
                .headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok()),
            Some(octo_user_agent::value())
        );
    }
}
