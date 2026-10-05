//! Port of `Services/Soulseek/SoulseekClient.cs`: the HTTP half. The records and the readers of
//! slskd's JSON are `octo_core::soulseek::soulseek_client`.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::{Clock, dotnet};
use octo_core::settings::SoulseekSettings;
use octo_core::soulseek::soulseek_client::{
    self as wire, BatchEnqueue, DirectoryError, SearchStatus, SoulseekFileHit, SoulseekTransferProgress,
    SoulseekTransferState, TransferWatch, contains_ignore_case,
};
use octo_core::soulseek::{SearchProfile, SoulseekServerReading};
use parking_lot::Mutex;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub use octo_core::soulseek::soulseek_client::{CANCEL_GRACE, MAX_TRANSFER_TIME, OPERATION_RETRIES};

/// What a Soulseek call that the C# let throw came to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SoulseekClientError {
    /// The caller gave up (`OperationCanceledException` from its token).
    #[error("The operation was canceled.")]
    Cancelled,
    /// The HTTP client's own 30 second timeout (`TaskCanceledException` without the caller's
    /// token), which the transfer wait let through.
    #[error("{0}")]
    Timeout(String),
    /// Any other failure, with the message the C# exception carried.
    #[error("{0}")]
    Failed(String),
}

/// The waits a test may shorten, and the clock a transfer's or a search's wait goes by.
#[derive(Clone)]
pub struct Timings {
    /// The least time between two search starts, so searches made side by side reach the
    /// Soulseek network spaced out rather than in a burst. Only tests shorten it.
    pub min_search_spacing: Duration,
    /// How often a transfer is polled. Only tests shorten it.
    pub poll_interval: Duration,
    /// How often a running search's state is read. Only tests shorten it.
    pub search_poll_interval: Duration,
    /// The wait before the one retry of a start slskd refused with 429. Only tests shorten it.
    pub search_start_retry_delay: Duration,
    /// The time a transfer's or a search's wait goes by. Only tests replace it.
    pub clock: Clock,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            min_search_spacing: Duration::from_secs(1),
            poll_interval: Duration::from_millis(1500),
            search_poll_interval: Duration::from_secs(1),
            search_start_retry_delay: Duration::from_secs(1),
            clock: Clock::system(),
        }
    }
}

/// Thin HTTP client for slskd's REST API. Handles auth and the small set of
/// endpoints Octo needs: search, browse responses, enqueue download, poll status.
///
/// Cheap to clone: the clones share one session, one operation gate and one batch memory.
#[derive(Clone)]
pub struct SoulseekClient {
    inner: Arc<Inner>,
}

struct Jwt {
    token: Option<String>,
    expires_utc: DateTime<Utc>,
}

struct Inner {
    http: reqwest::Client,
    /// `IOptions<SoulseekSettings>`: the address, login and download timeout are deliberately
    /// the values Octo started with (a restart-only setting; see RestartTracker).
    base: String,
    username: Option<String>,
    password: Option<String>,
    download_timeout_seconds: i32,

    jwt: Mutex<Jwt>,
    auth_lock: tokio::sync::Mutex<()>,

    // slskd runs one search start or one enqueue at a time and answers 429 to a second arriving in
    // the same moment. With downloads side by side Octo now makes those itself, so its own POSTs
    // queue here, and only the POST: never a wait on what it started.
    operation_gate: tokio::sync::Mutex<()>,
    last_search_start: Mutex<Option<Instant>>,

    /// Whether this slskd takes batch downloads (see [`SoulseekClient::batches_supported`]).
    batches_supported: Mutex<Option<bool>>,

    timings: Timings,

    /// The latest search's background cleanup. Only tests await it.
    last_search_cleanup: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
}

/// What kind of exception a failed call would have thrown, for the callers whose catch was
/// narrower than `Exception`.
enum Fault {
    /// `TaskCanceledException`: the HTTP client's timeout.
    Canceled,
    /// `HttpRequestException`.
    Http,
    /// `JsonException`.
    Json,
    /// Anything else (`InvalidOperationException`, `KeyNotFoundException`, ...).
    Other,
}

fn fault_of(error: &anyhow::Error) -> Fault {
    if let Some(e) = error.downcast_ref::<reqwest::Error>() {
        if e.is_timeout() {
            Fault::Canceled
        } else {
            Fault::Http
        }
    } else if error.downcast_ref::<serde_json::Error>().is_some() {
        Fault::Json
    } else {
        Fault::Other
    }
}

/// The message the C# exception carried, as near as reqwest can say it.
fn message(error: &anyhow::Error) -> String {
    match error.downcast_ref::<reqwest::Error>() {
        Some(e) if e.is_timeout() => {
            crate::services::http_client_factory::timeout_message(SoulseekClient::TIMEOUT)
        }
        Some(e) => crate::services::http_client_factory::connect_failure_message(e),
        None => error.to_string(),
    }
}

fn to_client_error(error: anyhow::Error) -> SoulseekClientError {
    match fault_of(&error) {
        Fault::Canceled => SoulseekClientError::Timeout(message(&error)),
        _ => SoulseekClientError::Failed(message(&error)),
    }
}

/// `EnsureSuccessStatusCode()`'s message.
fn not_success(status: StatusCode) -> String {
    format!(
        "Response status code does not indicate success: {} ({}).",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
}

/// The future, unless the token is cancelled first (an already-cancelled token wins at once, as
/// every .NET API that took one checked it before starting).
async fn cancellable<T>(
    ct: &CancellationToken,
    work: impl Future<Output = T>,
) -> Result<T, SoulseekClientError> {
    tokio::select! {
        biased;
        _ = ct.cancelled() => Err(SoulseekClientError::Cancelled),
        value = work => Ok(value),
    }
}

/// Removes a search from slskd once [`SoulseekClient::search`] is done with it, however it
/// ends: returned, given up through the token, or dropped (the C# `finally`).
struct SearchCleanup {
    client: SoulseekClient,
    search_id: String,
    started: bool,
    ended: bool,
}

impl Drop for SearchCleanup {
    fn drop(&mut self) {
        if !self.started {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        let search_id = std::mem::take(&mut self.search_id);
        let ended = self.ended;
        // In the background, so nobody waits on housekeeping.
        let task = runtime.spawn(async move { client.clean_up_search(&search_id, ended).await });
        *self.client.inner.last_search_cleanup.lock() = Some(task.map(|_| ()).boxed().shared());
    }
}

impl SoulseekClient {
    /// The HTTP client's timeout.
    pub const TIMEOUT: Duration = Duration::from_secs(30);

    /// `IOptions<SoulseekSettings>`: the settings are read once, here.
    pub fn new(settings: &SoulseekSettings) -> Self {
        Self::with_timings(settings, Timings::default())
    }

    /// A client whose waits and clock are the given ones (the C# tests set the internal
    /// properties).
    pub fn with_timings(settings: &SoulseekSettings, timings: Timings) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .no_gzip()
            .no_deflate()
            .build()
            .expect("a plain HTTP client builds");
        SoulseekClient {
            inner: Arc::new(Inner {
                http,
                base: settings
                    .base_url
                    .as_deref()
                    .unwrap_or("http://localhost:5030")
                    .trim_end_matches('/')
                    .to_string(),
                username: settings.username.clone(),
                password: settings.password.clone(),
                download_timeout_seconds: settings.download_timeout_seconds,
                jwt: Mutex::new(Jwt {
                    token: None,
                    expires_utc: DateTime::<Utc>::MIN_UTC,
                }),
                auth_lock: tokio::sync::Mutex::new(()),
                operation_gate: tokio::sync::Mutex::new(()),
                last_search_start: Mutex::new(None),
                batches_supported: Mutex::new(None),
                timings,
                last_search_cleanup: Mutex::new(None),
            }),
        }
    }

    /// slskd's address, as captured at startup.
    pub fn base_url(&self) -> &str {
        &self.inner.base
    }

    fn now(&self) -> DateTime<Utc> {
        self.inner.timings.clock.now()
    }

    fn token_fresh(&self) -> Option<String> {
        let jwt = self.inner.jwt.lock();
        let token = jwt.token.clone().filter(|t| !t.is_empty())?;
        let refresh_at = jwt.expires_utc.checked_sub_signed(TimeDelta::minutes(1))?;
        (Utc::now() < refresh_at).then_some(token)
    }

    /// Fetches and caches a JWT from slskd's session endpoint. Re-authenticates
    /// when the cached token is missing or near expiry.
    async fn get_jwt(&self) -> anyhow::Result<Option<String>> {
        if let Some(token) = self.token_fresh() {
            return Ok(Some(token));
        }

        let _auth = self.inner.auth_lock.lock().await;
        if let Some(token) = self.token_fresh() {
            return Ok(Some(token));
        }

        let (Some(username), Some(password)) = (
            self.inner.username.as_deref().filter(|u| !dotnet::is_blank(u)),
            self.inner.password.as_deref().filter(|p| !dotnet::is_blank(p)),
        ) else {
            warn!("Soulseek__Username/Password not set; cannot authenticate to slskd");
            return Ok(None);
        };

        let resp = self
            .inner
            .http
            .post(format!("{}/api/v0/session", self.inner.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(wire::session_payload(username, password))
            .send()
            .await?;

        if !resp.status().is_success() {
            warn!("slskd auth failed: HTTP {}", resp.status().as_u16());
            self.inner.jwt.lock().token = None;
            return Ok(None);
        }

        let json = resp.text().await?;
        let (token, expires) = wire::parse_session(&json)?;
        let mut jwt = self.inner.jwt.lock();
        jwt.token = token.clone();
        jwt.expires_utc = expires;
        Ok(token)
    }

    async fn authed_request(
        &self,
        method: &Method,
        url: &str,
        body: Option<&str>,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let mut req = self.inner.http.request(method.clone(), url);
        if let Some(jwt) = self.get_jwt().await?.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(jwt);
        }
        if let Some(body) = body {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
                .body(body.to_string());
        }
        Ok(req)
    }

    async fn send(&self, method: Method, url: &str, body: Option<&str>) -> anyhow::Result<reqwest::Response> {
        let resp = self.authed_request(&method, url, body).await?.send().await?;

        // If the JWT was rejected (e.g. rotated key), refresh once and retry.
        if resp.status() == StatusCode::UNAUTHORIZED {
            self.inner.jwt.lock().token = None;
            drop(resp);
            return Ok(self.authed_request(&method, url, body).await?.send().await?);
        }
        Ok(resp)
    }

    async fn get_text(&self, url: &str) -> anyhow::Result<Option<String>> {
        let resp = self.send(Method::GET, url, None).await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        Ok(Some(resp.text().await?))
    }

    pub async fn is_reachable(&self) -> bool {
        match self
            .send(
                Method::GET,
                &format!("{}/api/v0/application", self.inner.base),
                None,
            )
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("slskd not reachable at {}: {}", self.inner.base, message(&e));
                false
            }
        }
    }

    /// slskd's own word on the Soulseek network, from the same /api/v0/application call that
    /// proves slskd is up. During Soulseek's maintenance on 2026-10-03 slskd answered every call
    /// while sitting in "Disconnecting", and every search failed with "must be connected and
    /// logged in". None when slskd did not answer.
    pub async fn read_server(&self) -> Option<SoulseekServerReading> {
        match self
            .get_text(&format!("{}/api/v0/application", self.inner.base))
            .await
        {
            Ok(Some(json)) => Some(wire::parse_server_reading(&json)),
            Ok(None) => None,
            Err(e) => {
                debug!("slskd state not readable at {}: {}", self.inner.base, message(&e));
                None
            }
        }
    }

    /// Reads slskd's resolved downloads directory from /api/v0/options. Purely a
    /// diagnostic: a None (endpoint missing, redacted, or unexpected shape) must
    /// never gate anything.
    pub async fn get_downloads_directory(&self) -> Option<String> {
        self.get_directory_option("downloads").await
    }

    /// Where slskd writes a transfer before moving it to the downloads directory (#69). Only
    /// its last folder name is any use: the full path is slskd's view of its own container.
    /// None is normal and leaves slskd's default name in force.
    pub async fn get_incomplete_directory(&self) -> Option<String> {
        self.get_directory_option("incomplete").await
    }

    async fn get_directory_option(&self, name: &str) -> Option<String> {
        let read = async {
            let Some(json) = self
                .get_text(&format!("{}/api/v0/options", self.inner.base))
                .await?
            else {
                return Ok(None);
            };
            anyhow::Ok(wire::parse_directory_option(&json, name)?)
        };
        match read.await {
            Ok(value) => value,
            Err(e) => {
                debug!("Could not read slskd options: {}", message(&e));
                None
            }
        }
    }

    /// Runs one Soulseek search through slskd and returns every file it found.
    ///
    /// slskd keeps a running search's responses in memory and saves them only when the search
    /// ends (SearchService.cs, slskd 0.26.0), so /responses is empty until then and there is
    /// nothing to act on early. Octo reads the search's state until slskd ends it, then reads
    /// the responses once. A search still running at the profile's ceiling is cancelled, not
    /// abandoned: a cancelled search still saves what it gathered. Octo used to read nothing at
    /// the ceiling and delete the search, which returned zero hits and left the search running
    /// in slskd, because DELETE removes only the record.
    ///
    /// A caller who gives up still gets [`SoulseekClientError::Cancelled`], as before, so a
    /// cancelled acquisition is not mistaken for "not on Soulseek"; the slskd search is
    /// cancelled behind it. Dropping the future does the same.
    pub async fn search(
        &self,
        query: &str,
        profile: &SearchProfile,
        ct: &CancellationToken,
    ) -> Result<Vec<SoulseekFileHit>, SoulseekClientError> {
        let search_id = uuid::Uuid::new_v4().to_string();
        let began = self.now();
        // Set before the start goes out: a caller who gives up while it is on its way leaves a
        // search slskd may already have taken, and cancelling one it never made does no harm.
        let mut cleanup = SearchCleanup {
            client: self.clone(),
            search_id: search_id.clone(),
            started: true,
            ended: false,
        };
        let outcome = self
            .run_search(&search_id, query, profile, began, ct, &mut cleanup)
            .await;
        if outcome == Err(SoulseekClientError::Cancelled) {
            info!(
                "Soulseek search '{query}' ({}): given up after {:.1}s; cancelling it in slskd",
                profile.name,
                seconds_since(began, self.now())
            );
        }
        outcome
    }

    async fn run_search(
        &self,
        search_id: &str,
        query: &str,
        profile: &SearchProfile,
        began: DateTime<Utc>,
        ct: &CancellationToken,
        cleanup: &mut SearchCleanup,
    ) -> Result<Vec<SoulseekFileHit>, SoulseekClientError> {
        if !self.start_search(search_id, query, profile, ct).await? {
            cleanup.started = false;
            return Ok(Vec::new());
        }
        let ceiling = began + TimeDelta::seconds(i64::from(profile.ceiling_seconds));
        let mut status = self.wait_for_end(search_id, ceiling, ct).await?;
        let reason = if status.as_ref().is_some_and(|s| s.ended) {
            "finished"
        } else {
            self.cancel_search(search_id).await;
            let grace = self.now() + wire::delta(CANCEL_GRACE);
            status = self.wait_for_end(search_id, grace, ct).await?.or(status);
            if status.as_ref().is_some_and(|s| s.ended) {
                "ceiling, cancelled"
            } else {
                "ceiling, cancel not confirmed"
            }
        };
        cleanup.ended = status.as_ref().is_some_and(|s| s.ended);

        // Read even when the cancel was not confirmed: slskd may have finished since the last
        // look, and one request is cheap next to the wait already spent.
        let hits = self.read_responses(search_id, ct).await?;
        info!(
            "Soulseek search '{query}' ({}): {} hits after {:.1}s ({reason}; slskd {}, {} responses)",
            profile.name,
            hits.len(),
            seconds_since(began, self.now()),
            status.as_ref().map_or("unknown", |s| s.state.as_str()),
            status.as_ref().map_or(0, |s| s.response_count),
        );
        Ok(hits)
    }

    async fn start_search(
        &self,
        search_id: &str,
        query: &str,
        profile: &SearchProfile,
        ct: &CancellationToken,
    ) -> Result<bool, SoulseekClientError> {
        let url = format!("{}/api/v0/searches", self.inner.base);
        let body = wire::search_payload(search_id, query, profile);
        match cancellable(ct, self.send_operation(&url, &body, true)).await? {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(resp) => {
                warn!("Soulseek search start failed: {}", not_success(resp.status()));
                Ok(false)
            }
            Err(e) => {
                warn!("Soulseek search start failed: {}", message(&e));
                Ok(false)
            }
        }
    }

    /// One POST to slskd's one-at-a-time endpoints (search start, enqueue), through Octo's own gate
    /// and sent again on 429. A 429 that survives every retry is handed back for the caller to read.
    async fn send_operation(&self, url: &str, body: &str, search: bool) -> anyhow::Result<reqwest::Response> {
        let mut attempt: u32 = 1;
        loop {
            let resp = {
                let _gate = self.inner.operation_gate.lock().await;
                if search {
                    let last = *self.inner.last_search_start.lock();
                    if let Some(last) = last {
                        let wait = (last + self.inner.timings.min_search_spacing)
                            .saturating_duration_since(Instant::now());
                        if !wait.is_zero() {
                            tokio::time::sleep(wait).await;
                        }
                    }
                }
                let resp = self.send(Method::POST, url, Some(body)).await?;
                if search {
                    *self.inner.last_search_start.lock() = Some(Instant::now());
                }
                resp
            };
            if resp.status() != StatusCode::TOO_MANY_REQUESTS || attempt > OPERATION_RETRIES {
                return Ok(resp);
            }
            drop(resp);
            tokio::time::sleep(self.inner.timings.search_start_retry_delay * attempt).await;
            attempt += 1;
        }
    }

    /// Reads the search's state until it has ended or `until` passes. Returns the last state
    /// read, or None when none could be read.
    async fn wait_for_end(
        &self,
        search_id: &str,
        until: DateTime<Utc>,
        ct: &CancellationToken,
    ) -> Result<Option<SearchStatus>, SoulseekClientError> {
        let mut last: Option<SearchStatus> = None;
        while self.now() < until {
            cancellable(ct, tokio::time::sleep(self.inner.timings.search_poll_interval)).await?;
            last = self.read_search_status(search_id, ct).await?.or(last);
            if last.as_ref().is_some_and(|s| s.ended) {
                break;
            }
        }
        Ok(last)
    }

    async fn read_search_status(
        &self,
        search_id: &str,
        ct: &CancellationToken,
    ) -> Result<Option<SearchStatus>, SoulseekClientError> {
        let url = format!("{}/api/v0/searches/{search_id}", self.inner.base);
        let read = async {
            match self.get_text(&url).await? {
                Some(json) => anyhow::Ok(wire::parse_search_status(&json)?),
                None => Ok(None),
            }
        };
        Ok(match cancellable(ct, read).await? {
            Ok(status) => status,
            Err(e) => {
                debug!("Soulseek search state read failed (transient): {}", message(&e));
                None
            }
        })
    }

    async fn cancel_search(&self, search_id: &str) {
        let url = format!("{}/api/v0/searches/{search_id}", self.inner.base);
        if let Err(e) = self.send(Method::PUT, &url, None).await {
            debug!("Soulseek search cancel failed: {}", message(&e));
        }
    }

    async fn read_responses(
        &self,
        search_id: &str,
        ct: &CancellationToken,
    ) -> Result<Vec<SoulseekFileHit>, SoulseekClientError> {
        let url = format!("{}/api/v0/searches/{search_id}/responses", self.inner.base);
        Ok(match cancellable(ct, self.get_text(&url)).await? {
            Ok(Some(json)) => {
                let (hits, error) = wire::parse_responses(&json);
                if let Some(error) = error {
                    warn!("Failed to parse Soulseek responses: {error}");
                }
                hits
            }
            Ok(None) => Vec::new(),
            Err(e) => {
                warn!("Soulseek search responses could not be read: {}", message(&e));
                Vec::new()
            }
        })
    }

    /// Removes the search from slskd. One that has not ended is cancelled first: DELETE removes
    /// only the record, and the search would carry on asking the network for nobody. The short
    /// wait after the cancel lets slskd save the ended search before its record goes, so that
    /// save does not fail in slskd's log.
    async fn clean_up_search(&self, search_id: &str, ended: bool) {
        if !ended {
            self.cancel_search(search_id).await;
            let grace = self.now() + wire::delta(CANCEL_GRACE);
            // Nobody can give this up: the token is never cancelled.
            let _ = self
                .wait_for_end(search_id, grace, &CancellationToken::new())
                .await;
        }
        let url = format!("{}/api/v0/searches/{search_id}", self.inner.base);
        if let Err(e) = self.send(Method::DELETE, &url, None).await {
            debug!("Soulseek search cleanup failed: {}", message(&e));
        }
    }

    /// The latest search's background cleanup, for a test to wait on.
    pub async fn last_search_cleanup(&self) {
        let cleanup = self.inner.last_search_cleanup.lock().clone();
        if let Some(cleanup) = cleanup {
            cleanup.await;
        }
    }

    /// The files in one folder of a peer's share (slskd asks the peer for that folder alone, not
    /// its whole share). Each comes back with its full remote path, ready to enqueue, and the
    /// queue and speed of `from`, the search hit that pointed at the folder.
    /// Empty when the peer does not answer in time or will not list it. An answer with a field
    /// of the wrong kind is an error, as it escaped the C#'s narrower catch.
    pub async fn browse_folder(
        &self,
        from: &SoulseekFileHit,
        directory: &str,
        timeout: Duration,
    ) -> anyhow::Result<Vec<SoulseekFileHit>> {
        let url = format!(
            "{}/api/v0/users/{}/directory",
            self.inner.base,
            dotnet::escape_data_string(&from.username)
        );
        let body = wire::browse_payload(directory);
        let read = async {
            let resp = self.send_operation(&url, &body, false).await?;
            let status = resp.status();
            if !status.is_success() {
                return anyhow::Ok(Err(status));
            }
            Ok(Ok(resp.text().await?))
        };
        let did_not_list = || {
            info!(
                "{} did not list the folder {directory} within {}s",
                from.username,
                timeout.as_secs_f64()
            );
            Ok(Vec::new())
        };
        let could_not_list = |text: String| {
            info!("Could not list {}'s folder {directory}: {text}", from.username);
            Ok(Vec::new())
        };
        match tokio::time::timeout(timeout, read).await {
            Err(_) => did_not_list(),
            Ok(Err(e)) => match fault_of(&e) {
                Fault::Canceled => did_not_list(),
                Fault::Http | Fault::Json => could_not_list(message(&e)),
                Fault::Other => Err(e),
            },
            Ok(Ok(Err(status))) => {
                info!(
                    "slskd could not list {}'s folder {directory}: HTTP {}",
                    from.username,
                    status.as_u16()
                );
                Ok(Vec::new())
            }
            Ok(Ok(Ok(json))) => match wire::parse_directory(&json, from, directory) {
                Ok(hits) => Ok(hits),
                Err(DirectoryError::Json(e)) => could_not_list(e.to_string()),
                Err(DirectoryError::Element(e)) => Err(e.into()),
            },
        }
    }

    /// Enqueues a download from a specific peer. Returns when the request is accepted by slskd
    /// (not when the file is fully transferred — caller polls for that).
    pub async fn enqueue_download(
        &self,
        username: &str,
        filename: &str,
        size: i64,
    ) -> Result<(), SoulseekClientError> {
        let url = format!(
            "{}/api/v0/transfers/downloads/{}",
            self.inner.base,
            dotnet::escape_data_string(username)
        );
        let resp = self
            .send_operation(&url, &wire::enqueue_payload(filename, size), false)
            .await
            .map_err(to_client_error)?;

        let status = resp.status();
        if !status.is_success() {
            let err = resp.text().await.map_err(|e| to_client_error(e.into()))?;
            return Err(SoulseekClientError::Failed(format!(
                "slskd download enqueue failed: HTTP {} {err}",
                status.as_u16()
            )));
        }
        Ok(())
    }

    /// Whether this slskd takes batch downloads, which is what lets each download land in a folder
    /// of Octo's choosing. None until the first batch has been tried. True once one was accepted,
    /// and from then on an error is an error. False once slskd answered the batch route as if it
    /// did not know it, and from then on Octo enqueues the old way without asking again.
    pub fn batches_supported(&self) -> Option<bool> {
        *self.inner.batches_supported.lock()
    }

    pub fn set_batches_supported(&self, supported: Option<bool>) {
        *self.inner.batches_supported.lock() = supported;
    }

    /// Queues files from one peer as one slskd batch, all landing in `destination`,
    /// a folder relative to slskd's downloads directory. The answer carries each file's transfer id,
    /// so the wait can follow that transfer rather than any transfer of the same file name.
    ///
    /// An slskd older than batches answers this route through its per-user enqueue (the user named
    /// "batches"), which rejects the body with 400, or with 404 or 405. Before any batch has worked,
    /// those mean "no batches here"; after one has, they are real errors.
    pub async fn enqueue_batch(
        &self,
        username: &str,
        files: &[(String, i64)],
        destination: &str,
    ) -> Result<BatchEnqueue, SoulseekClientError> {
        if self.batches_supported() == Some(false) {
            return Ok(BatchEnqueue::not_supported());
        }
        let url = format!("{}/api/v0/transfers/downloads/batches", self.inner.base);
        let resp = self
            .send_operation(&url, &wire::batch_payload(username, files, destination), false)
            .await
            .map_err(to_client_error)?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| to_client_error(e.into()))?;
        if status.is_success() {
            self.set_batches_supported(Some(true));
            return wire::parse_batch(&body).map_err(|e| SoulseekClientError::Failed(e.to_string()));
        }
        if self.batches_supported() != Some(true)
            && matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            )
        {
            self.set_batches_supported(Some(false));
            info!(
                "slskd does not take batch downloads (HTTP {}); downloads go one at a time, the old way",
                status.as_u16()
            );
            return Ok(BatchEnqueue::not_supported());
        }
        Err(SoulseekClientError::Failed(format!(
            "slskd batch enqueue failed: HTTP {} {body}",
            status.as_u16()
        )))
    }

    /// Polls a download to completion. Returns Succeeded on success or Errored
    /// on any kind of slskd-side failure (peer rejected, timed out, cancelled,
    /// or the transfer silently disappeared from slskd's active list — which
    /// happens after rejection on some slskd versions and would otherwise hang
    /// us forever). Caller decides whether to retry or escalate.
    ///
    /// `on_progress` hears the transfer's byte counts on every poll that finds
    /// it. It only listens: the cadence, the deadline and the outcome are the same without it.
    ///
    /// The caller giving up through `ct` is [`SoulseekClientError::Cancelled`]; a poll that hit
    /// the HTTP client's own timeout is [`SoulseekClientError::Timeout`], which the C# let
    /// through as a `TaskCanceledException`.
    pub async fn wait_for_completion(
        &self,
        username: &str,
        filename: &str,
        per_attempt_timeout_seconds: Option<i32>,
        ct: &CancellationToken,
        on_progress: Option<&(dyn Fn(&SoulseekTransferProgress) + Send + Sync)>,
        transfer: Option<&str>,
    ) -> Result<SoulseekTransferState, SoulseekClientError> {
        let timeout_sec = per_attempt_timeout_seconds.unwrap_or(self.inner.download_timeout_seconds);
        let quiet = Duration::from_secs(u64::try_from(timeout_sec).unwrap_or(0));
        let mut watch = TransferWatch::new(self.now(), quiet, MAX_TRANSFER_TIME);
        let mut seen_at_least_once = false;
        let mut consecutive_misses = 0;
        let url = format!(
            "{}/api/v0/transfers/downloads/{}",
            self.inner.base,
            dotnet::escape_data_string(username)
        );

        while !watch.expired(self.now()) && !ct.is_cancelled() {
            cancellable(ct, tokio::time::sleep(self.inner.timings.poll_interval)).await?;

            let resp = match cancellable(ct, self.send(Method::GET, &url, None)).await? {
                Ok(resp) => resp,
                Err(e) => {
                    if matches!(fault_of(&e), Fault::Canceled) {
                        return Err(SoulseekClientError::Timeout(message(&e)));
                    }
                    debug!("Transfer poll transient: {}", message(&e));
                    continue;
                }
            };
            if !resp.status().is_success() {
                if seen_at_least_once {
                    consecutive_misses += 1;
                }
                if consecutive_misses >= wire::MAX_CONSECUTIVE_MISSES_AFTER_SEEN {
                    warn!("slskd transfer disappeared after rejection (no longer queryable): {filename}");
                    return Ok(SoulseekTransferState::Errored);
                }
                continue;
            }

            let json = match cancellable(ct, resp.text()).await? {
                Ok(json) => json,
                Err(e) => {
                    let e = anyhow::Error::from(e);
                    if matches!(fault_of(&e), Fault::Canceled) {
                        return Err(SoulseekClientError::Timeout(message(&e)));
                    }
                    debug!("Transfer poll transient: {}", message(&e));
                    continue;
                }
            };
            let poll = (|| -> anyhow::Result<Option<(String, SoulseekTransferProgress)>> {
                let doc: Value = serde_json::from_str(&json)?;
                let Some(found) = wire::find_transfer(&doc, filename, transfer)? else {
                    return Ok(None);
                };
                let state = wire::state_of(found)?;
                Ok(Some((state, wire::read_transfer_progress(found)?)))
            })();
            let found = match poll {
                Ok(found) => found,
                Err(e) => {
                    debug!("Transfer poll transient: {e}");
                    continue;
                }
            };

            match found {
                Some((state, progress)) => {
                    seen_at_least_once = true;
                    consecutive_misses = 0;
                    watch.saw(progress.bytes_transferred, self.now());
                    if let Some(listener) = on_progress {
                        listener(&progress);
                    }

                    if contains_ignore_case(&state, "Completed") && contains_ignore_case(&state, "Succeeded")
                    {
                        return Ok(SoulseekTransferState::Succeeded);
                    }
                    if ["Errored", "Cancelled", "Rejected", "TimedOut"]
                        .iter()
                        .any(|word| contains_ignore_case(&state, word))
                    {
                        debug!("slskd transfer ended in state: {state}");
                        return Ok(SoulseekTransferState::Errored);
                    }
                }
                None if seen_at_least_once => {
                    consecutive_misses += 1;
                    if consecutive_misses >= wire::MAX_CONSECUTIVE_MISSES_AFTER_SEEN {
                        warn!(
                            "slskd transfer disappeared from active list (rejection or cleanup): {filename}"
                        );
                        return Ok(SoulseekTransferState::Errored);
                    }
                }
                None => {}
            }
        }

        if ct.is_cancelled() {
            return Err(SoulseekClientError::Cancelled);
        }

        // Giving up on this peer. Without a cancel slskd keeps the transfer going, and
        // a file that lands after the next peer's copy is a second copy in the library.
        if self.cancel_transfer(username, filename, transfer).await == SoulseekTransferState::Succeeded {
            info!("slskd transfer finished just as it was given up: {filename}");
            return Ok(SoulseekTransferState::Succeeded);
        }
        if watch.hit_ceiling(self.now()) {
            warn!(
                "slskd transfer still not done after {} minutes; cancelled: {filename}",
                MAX_TRANSFER_TIME.as_secs() / 60
            );
        } else {
            warn!("slskd transfer timed out: nothing new for {timeout_sec}s; cancelled: {filename}");
        }
        Ok(SoulseekTransferState::Errored)
    }

    /// Cancels a download in slskd and removes it from its list, so it can never land.
    /// Answers Succeeded instead when the transfer turns out to have just finished, and
    /// Errored otherwise, including when slskd cannot be asked.
    pub async fn cancel_transfer(
        &self,
        username: &str,
        filename: &str,
        transfer: Option<&str>,
    ) -> SoulseekTransferState {
        let user = dotnet::escape_data_string(username);
        let attempt = async {
            let Some(json) = self
                .get_text(&format!("{}/api/v0/transfers/downloads/{user}", self.inner.base))
                .await?
            else {
                return anyhow::Ok(SoulseekTransferState::Errored);
            };
            let doc: Value = serde_json::from_str(&json)?;
            let Some(file) = wire::find_transfer(&doc, filename, transfer)? else {
                return Ok(SoulseekTransferState::Errored);
            };
            let state = wire::state_of(file)?;
            if contains_ignore_case(&state, "Completed") && contains_ignore_case(&state, "Succeeded") {
                return Ok(SoulseekTransferState::Succeeded);
            }
            let Some(id) = wire::transfer_id(file)? else {
                return Ok(SoulseekTransferState::Errored);
            };
            let cancel = self
                .send(
                    Method::DELETE,
                    &format!(
                        "{}/api/v0/transfers/downloads/{user}/{}?remove=true",
                        self.inner.base,
                        dotnet::escape_data_string(&id)
                    ),
                    None,
                )
                .await?;
            if !cancel.status().is_success() {
                warn!(
                    "slskd refused to cancel {filename}: HTTP {}",
                    cancel.status().as_u16()
                );
            }
            Ok(SoulseekTransferState::Errored)
        };
        match attempt.await {
            Ok(state) => state,
            Err(e) => {
                warn!("Could not cancel slskd transfer {filename}: {}", message(&e));
                SoulseekTransferState::Errored
            }
        }
    }
}

fn seconds_since(began: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    (now - began).num_milliseconds() as f64 / 1000.0
}

#[cfg(test)]
#[path = "soulseek_client_tests.rs"]
mod tests;
