//! Port of `Services/Metadata/DeezerRateLimitHandler.cs`, together with the named "deezer"
//! `HttpClient` it sat in.
//!
//! In the C#, every Deezer caller resolved the named client from `IHttpClientFactory` and the
//! handler in its chain spent a permit. Here the client and the handler are one type: the only
//! way to reach Deezer is through [`DeezerRateLimitHandler::send`] (or [`get`](DeezerRateLimitHandler::get),
//! which adds the callers' 8 s timeout), which waits on the limiter first.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::StatusCode;
use reqwest::header::ACCEPT_LANGUAGE;
use tracing::warn;

use super::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;

/// Spends a [`DeezerRateLimiter`] permit before each Deezer API call. The limiter is the
/// singleton; this is the seam that consults it.
pub struct DeezerRateLimitHandler {
    client: reqwest::Client,
    limiter: Arc<DeezerRateLimiter>,
    api_host: String,
}

impl DeezerRateLimitHandler {
    pub const API_HOST: &'static str = "api.deezer.com";

    /// Both callers set `Timeout = 8 s` on the client they made. It covers waiting for a
    /// permit as well as the call and reading the answer.
    pub const TIMEOUT: Duration = Duration::from_secs(8);

    pub fn new(limiter: Arc<DeezerRateLimiter>) -> Self {
        Self::with_api_host(limiter, Self::API_HOST)
    }

    /// A handler that meters another host as the API: a test's mock server.
    pub fn with_api_host(limiter: Arc<DeezerRateLimiter>, api_host: impl Into<String>) -> Self {
        let client = client_builder().build().expect("the Deezer HTTP client builds");
        Self {
            client,
            limiter,
            api_host: api_host.into(),
        }
    }

    pub fn limiter(&self) -> &Arc<DeezerRateLimiter> {
        &self.limiter
    }

    /// `SendAsync`. `background` is the `BackgroundLane` request option: it marks a request
    /// as background work, so cache warming yields to anything a user is actually waiting on.
    pub async fn send(
        &self,
        request: reqwest::Request,
        background: bool,
    ) -> reqwest::Result<reqwest::Response> {
        // Only the API is metered. The cover-art lookup pulls actual image bytes from
        // cdn-images.dzcdn.net through this same client, and that host has no quota:
        // metering it would spend an API permit per rendered row and throttle a CDN for
        // nothing.
        let metered = request
            .url()
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.api_host));
        if !metered {
            return self.client.execute(request).await;
        }

        let lease = self.limiter.acquire(background).await;
        if !lease.is_acquired() {
            // The queue is full. Answering 429 rather than throwing means callers take
            // their existing "Deezer refused this" path, which never caches the result —
            // so back-pressure can slow us down but can never poison the cache.
            let lane = if background { "background" } else { "interactive" };
            warn!(
                "deezer rate limiter rejected a {lane} request to {}",
                request.url()
            );
            return Ok(too_many_requests());
        }

        self.client.execute(request).await
    }

    /// A GET as the C# callers made it: the given Accept-Language, the body read, and all of
    /// it (the permit included) inside [`Self::TIMEOUT`].
    pub async fn get(
        &self,
        url: &str,
        background: bool,
        accept_language: Option<&str>,
    ) -> anyhow::Result<HttpAnswer> {
        let mut builder = self.client.get(url);
        if let Some(language) = accept_language {
            builder = builder.header(ACCEPT_LANGUAGE, language);
        }
        let request = builder.build()?;
        let call = async {
            let response = self.send(request, background).await?;
            HttpAnswer::read(response).await
        };
        match tokio::time::timeout(Self::TIMEOUT, call).await {
            Ok(answer) => Ok(answer?),
            Err(_) => Err(anyhow!(
                "The request was canceled due to the configured HttpClient.Timeout of 8 seconds elapsing."
            )),
        }
    }
}

fn too_many_requests() -> reqwest::Response {
    let response = http::Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .body(Vec::<u8>::new())
        .expect("a bodiless 429 builds");
    reqwest::Response::from(response)
}

#[cfg(test)]
#[path = "deezer_rate_limit_handler_tests.rs"]
mod tests;
