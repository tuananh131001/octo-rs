//! Port of `Services/Subsonic/CredentialCheck.cs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use octo_core::common::SingleFlight;
use octo_subsonic::SubsonicCredential;
use tracing::warn;

use super::expiring_cache::ExpiringCache;
use super::subsonic_proxy_service::{RelayError, SubsonicProxyService};

/// What Navidrome said of a sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialVerdict {
    Accepted,
    Refused,
    Unreachable,
}

pub(crate) const ACCEPTED_LIFETIME: Duration = Duration::from_secs(10 * 60);
pub(crate) const CAPACITY: usize = 2048;
const WARN_EVERY: Duration = Duration::from_secs(60);

/// Asks Navidrome whether a sign-in is good, for requests Octo answers without relaying: a
/// star that starts a download and a stream of an outside song. Navidrome checks everything
/// relayed to it, but it never sees those ids, so before this nothing checked them at all.
///
/// Only the sign-in goes to Navidrome, as a ping. An answer is kept under a SHA-256 of the
/// sign-in: a yes for ten minutes, so a player asking for the same address again in ranges
/// pings once, and a no for half a minute, so a wrong password cannot make Octo ask on every
/// retry. An unreachable Navidrome, or one that takes more than five seconds to answer, is
/// never kept. Requests with one sign-in arriving together make one call between them.
pub struct CredentialCheck {
    inner: Arc<Inner>,
    asking: SingleFlight<String, CredentialVerdict>,
}

struct Inner {
    verdicts: ExpiringCache<CredentialVerdict>,
    refused_lifetime: Duration,
    /// How long a ping may take before Navidrome counts as unreachable. A request waits on
    /// it, and a player gives up long before a plain HTTP timeout would.
    check_timeout: Duration,
    /// When the last warning went out, in milliseconds since `epoch` plus one (0 = never).
    last_warning: AtomicU64,
    epoch: Instant,
}

impl Default for CredentialCheck {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialCheck {
    pub fn new() -> Self {
        Self::with_timings(Duration::from_secs(30), Duration::from_secs(5))
    }

    /// With the refused lifetime and the check timeout the tests shorten.
    pub(crate) fn with_timings(refused_lifetime: Duration, check_timeout: Duration) -> Self {
        CredentialCheck {
            inner: Arc::new(Inner {
                verdicts: ExpiringCache::new(CAPACITY),
                refused_lifetime,
                check_timeout,
                last_warning: AtomicU64::new(0),
                epoch: Instant::now(),
            }),
            asking: SingleFlight::new(),
        }
    }

    pub fn check_timeout(&self) -> Duration {
        self.inner.check_timeout
    }

    /// Navidrome's answer for this sign-in. None at all is refused without asking. The relay
    /// is the request's own, as with `RequestIdentity`. A caller that gives up drops the
    /// future; the call it waited on carries on for the others.
    ///
    /// `Err` only for a failure the C# did not catch either (a URL HttpClient cannot use at
    /// all), which reached the caller as an exception.
    pub async fn check(
        &self,
        credential: Option<&SubsonicCredential>,
        relay: &SubsonicProxyService,
    ) -> Result<CredentialVerdict, RelayError> {
        let Some(credential) = credential else {
            return Ok(CredentialVerdict::Refused);
        };
        let slot = credential.fingerprint().to_string();
        if let Some(known) = self.inner.verdicts.get(&slot) {
            return Ok(known);
        }
        let inner = Arc::clone(&self.inner);
        let credential = credential.clone();
        let relay = relay.clone();
        let key = slot.clone();
        let asking = self.asking.run(
            slot,
            move |_| async move {
                inner
                    .ask(&key, &credential, &relay)
                    .await
                    .map_err(anyhow::Error::new)
            },
            Duration::from_secs(3600),
        );
        asking.await.map_err(|error| {
            error
                .downcast_ref::<RelayError>()
                .cloned()
                .unwrap_or_else(|| RelayError::InvalidOperation(error.to_string()))
        })
    }

    /// `subsonic-response.status` of a JSON answer, or `None` when it is not one.
    pub(crate) fn status(body: &[u8]) -> Option<String> {
        let document: serde_json::Value = serde_json::from_slice(body).ok()?;
        match document.get("subsonic-response")?.get("status")? {
            serde_json::Value::String(status) => Some(status.clone()),
            _ => None,
        }
    }
}

impl Inner {
    async fn ask(
        &self,
        slot: &str,
        credential: &SubsonicCredential,
        relay: &SubsonicProxyService,
    ) -> Result<CredentialVerdict, RelayError> {
        if let Some(known) = self.verdicts.get(slot) {
            return Ok(known);
        }
        // The relay takes no token, so the wait is what is bounded; a late answer is dropped.
        let answer = tokio::time::timeout(
            self.check_timeout,
            relay.relay("rest/ping", &credential.parameters(&[])),
        )
        .await;
        let body = match answer {
            Err(_) => {
                self.warn_unreachable("TimeoutException");
                return Ok(CredentialVerdict::Unreachable);
            }
            Ok(Err(
                error @ (RelayError::Http(_) | RelayError::Canceled(_) | RelayError::NotConfigured(_)),
            )) => {
                self.warn_unreachable(error.type_name());
                return Ok(CredentialVerdict::Unreachable);
            }
            Ok(Err(other)) => return Err(other),
            Ok(Ok(response)) => response.body,
        };
        let verdict = match CredentialCheck::status(&body).as_deref() {
            Some("ok") => CredentialVerdict::Accepted,
            Some("failed") => CredentialVerdict::Refused,
            _ => CredentialVerdict::Unreachable,
        };
        // Not a Subsonic answer at all: the URL points somewhere else. Not the user's fault.
        if verdict == CredentialVerdict::Unreachable {
            self.warn_unreachable("an answer that was not Navidrome's");
            return Ok(verdict);
        }
        let lifetime = if verdict == CredentialVerdict::Accepted {
            ACCEPTED_LIFETIME
        } else {
            self.refused_lifetime
        };
        self.verdicts.set(slot, verdict, lifetime, None);
        Ok(verdict)
    }

    fn warn_unreachable(&self, reason: &str) {
        // Once a minute at most: a player retrying through an outage would fill the log.
        let now = self.epoch.elapsed().as_millis() as u64 + 1;
        let last = self.last_warning.load(Ordering::SeqCst);
        if last != 0 && now - last < WARN_EVERY.as_millis() as u64 {
            return;
        }
        if self
            .last_warning
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        warn!(
            "Octo could not reach Navidrome to check a sign-in ({reason}), so outside songs are refused until it can"
        );
    }
}

#[cfg(test)]
#[path = "credential_check_tests.rs"]
mod tests;
