//! Port of `Services/Subsonic/RequestIdentity.cs`.

use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use octo_core::common::SingleFlight;
use sha2::{Digest, Sha256};
use tracing::debug;

use super::expiring_cache::ExpiringCache;
use super::subsonic_proxy_service::{RelayError, SubsonicProxyService};

/// How long a key's username is kept. A key can be revoked, so not for long.
pub(crate) const LIFETIME: Duration = Duration::from_secs(5 * 60);

/// Keys remembered at once. A household has a handful; this only bounds a flood.
pub(crate) const CAPACITY: usize = 512;

/// Who a Subsonic request is from, for the things Octo keeps per listener: what radio learns
/// from a scrobble, whose Last.fm a play goes to, and whose search a later page continues.
///
/// A token or password sign-in names the user in `u`. An OpenSubsonic API key sign-in does
/// not: Navidrome refuses `u` next to `apiKey` (error 43), so the key is all there is. For
/// those, Navidrome is asked through `tokenInfo` (the apiKeyAuthentication extension) with the
/// same key, and the answer is kept for a few minutes.
///
/// Callers ask only once the request's credentials have already been accepted upstream, so an
/// unknown key never costs more than the one call that refuses it. When tokenInfo fails or
/// will not say, that too is kept, briefly, so a Navidrome without it is not asked on every
/// request; and requests with one key arriving together make one call between them. The key
/// itself is never stored or logged: the kept answer is filed under a SHA-256 of it.
pub struct RequestIdentity {
    inner: Arc<Inner>,
    /// tokenInfo calls still out, by key hash, for the requests that arrive meanwhile.
    asking: SingleFlight<String, Option<String>>,
}

struct Inner {
    /// What was learned of one key: its owner, or `None` when Navidrome would not say.
    names: ExpiringCache<Option<String>>,
    /// How long a key tokenInfo could not name is left unasked. Short: the next answer may
    /// well be different, and until then the request is simply nobody's.
    unnamed_lifetime: Duration,
}

impl Default for RequestIdentity {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestIdentity {
    pub fn new() -> Self {
        Self::with_unnamed_lifetime(Duration::from_secs(30))
    }

    pub(crate) fn with_unnamed_lifetime(unnamed_lifetime: Duration) -> Self {
        RequestIdentity {
            inner: Arc::new(Inner {
                names: ExpiringCache::new(CAPACITY),
                unnamed_lifetime,
            }),
            asking: SingleFlight::new(),
        }
    }

    /// The request's username: `u` when it has one, else the owner of its API key as
    /// Navidrome reports it, else `None`. `None` also when Navidrome could not say, so a caller
    /// skips what it keeps per user rather than filing it under nobody. The relay is the
    /// request's own (it is scoped to the request, where this is kept across requests). A
    /// caller that gives up drops the future; the call carries on for the others.
    ///
    /// `Err` only for a failure the C# did not catch either (a URL HttpClient cannot use).
    pub async fn username(
        &self,
        parameters: &IndexMap<String, String>,
        relay: &SubsonicProxyService,
    ) -> Result<Option<String>, RelayError> {
        if let Some(named) = parameters.get("u") {
            let username = named.trim();
            if !username.is_empty() {
                return Ok(Some(username.to_string()));
            }
        }
        let Some(api_key) = parameters.get("apiKey").filter(|k| !k.is_empty()) else {
            return Ok(None);
        };

        let slot = fingerprint(api_key);
        if let Some(known) = self.inner.names.get(&slot) {
            return Ok(known);
        }

        let mut ask = IndexMap::new();
        ask.insert("apiKey".to_string(), api_key.clone());
        ask.insert("f".to_string(), "json".to_string());
        for name in ["v", "c"] {
            if let Some(value) = parameters.get(name).filter(|v| !v.is_empty()) {
                ask.insert(name.to_string(), value.clone());
            }
        }
        // One call per key however many requests wait on it. It is not tied to any one of
        // them, so a request that gives up does not fail the others.
        let inner = Arc::clone(&self.inner);
        let relay = relay.clone();
        let key = slot.clone();
        let asking = self.asking.run(
            slot,
            move |_| async move { inner.ask(&key, &ask, &relay).await.map_err(anyhow::Error::new) },
            Duration::from_secs(3600),
        );
        asking.await.map_err(|error| {
            error
                .downcast_ref::<RelayError>()
                .cloned()
                .unwrap_or_else(|| RelayError::InvalidOperation(error.to_string()))
        })
    }

    /// `tokenInfo.username` from an ok answer, or `None`.
    pub(crate) fn token_info_username(body: &[u8]) -> Option<String> {
        let document: serde_json::Value = serde_json::from_slice(body).ok()?;
        let response = document.get("subsonic-response")?;
        if response.get("status")?.as_str()? != "ok" {
            return None;
        }
        let username = response.get("tokenInfo")?.get("username")?.as_str()?.trim();
        (!username.is_empty()).then(|| username.to_string())
    }

    /// Keys being remembered, for tests.
    #[cfg(test)]
    pub(crate) fn remembered(&self) -> usize {
        self.inner.names.len()
    }
}

impl Inner {
    async fn ask(
        &self,
        slot: &str,
        ask: &IndexMap<String, String>,
        relay: &SubsonicProxyService,
    ) -> Result<Option<String>, RelayError> {
        // Answered while this call was being set up.
        if let Some(known) = self.names.get(slot) {
            return Ok(known);
        }
        let owner = match relay.relay("rest/tokenInfo", ask).await {
            Ok(response) => {
                let owner = RequestIdentity::token_info_username(&response.body);
                if owner.is_none() {
                    debug!("Navidrome did not say whose API key this request used");
                }
                owner
            }
            Err(error @ (RelayError::Http(_) | RelayError::Canceled(_) | RelayError::NotConfigured(_))) => {
                debug!(
                    "Could not ask Navidrome whose API key this request used: {}",
                    error.type_name()
                );
                None
            }
            Err(other) => return Err(other),
        };
        let lifetime = if owner.is_none() {
            self.unnamed_lifetime
        } else {
            LIFETIME
        };
        self.names.set(slot, owner.clone(), lifetime, None);
        Ok(owner)
    }
}

fn fingerprint(api_key: &str) -> String {
    hex::encode_upper(Sha256::digest(api_key.as_bytes()))
}

#[cfg(test)]
#[path = "request_identity_tests.rs"]
mod tests;
