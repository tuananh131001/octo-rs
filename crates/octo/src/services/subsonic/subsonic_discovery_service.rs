//! Port of `Services/Subsonic/SubsonicDiscoveryService.cs`.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Semaphore;
use tracing::info;

/// One Subsonic-compatible server found on the local network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredServer {
    pub url: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub server_version: Option<String>,
    pub requires_auth: bool,
}

// Navidrome default is 4533; 4040 is Airsonic-Advanced; 4747/8080 are common
// reverse-proxy/self-host choices. Kept short so the sweep stays fast.
const CANDIDATE_PORTS: [u16; 4] = [4533, 4040, 4747, 8080];
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_CONCURRENCY: usize = 96;

/// Finds Subsonic/Navidrome servers on the local network so Octo, which is an accessory to an
/// existing server, can auto-configure its upstream URL instead of making the user hand-type
/// it. The probe is credential-free: a Subsonic ping to a real server returns a
/// `{"subsonic-response":...}` envelope (even with no/bad auth, as a `code 40` failure), while
/// a non-Subsonic host returns 404/HTML. The envelope also carries the server `type` and
/// `serverVersion` for display.
///
/// Scope is the host's own /24 on a small set of well-known ports, run concurrently with short
/// timeouts, so a full sweep takes a few seconds. Only works where Octo can see the real LAN
/// (host networking or an LXC); in a Docker bridge network it only sees the internal subnet
/// and will typically find nothing.
#[derive(Default)]
pub struct SubsonicDiscoveryService;

impl SubsonicDiscoveryService {
    pub fn new() -> Self {
        SubsonicDiscoveryService
    }

    pub async fn scan(&self) -> Vec<DiscoveredServer> {
        let targets = build_targets();
        if targets.is_empty() {
            info!("Server discovery: no scannable local /24 found (bridge network?).");
            return Vec::new();
        }
        info!(
            "Server discovery: probing {} hosts x {} ports...",
            targets.len(),
            CANDIDATE_PORTS.len()
        );
        let found = probe_all(&targets, &CANDIDATE_PORTS).await;
        info!("Server discovery: found {} Subsonic server(s).", found.len());
        found
    }
}

/// Probes every host and port, at most [`MAX_CONCURRENCY`] at once, and returns the servers
/// found, each URL once (ignoring case), ordered by URL ignoring case.
async fn probe_all(hosts: &[String], ports: &[u16]) -> Vec<DiscoveredServer> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .no_gzip()
        .no_deflate()
        .build()
        .expect("the probe client builds");
    let gate = Arc::new(Semaphore::new(MAX_CONCURRENCY));
    let found: Arc<Mutex<(Vec<DiscoveredServer>, HashSet<String>)>> = Arc::default();
    let mut tasks = tokio::task::JoinSet::new();
    for host in hosts {
        for &port in ports {
            let Ok(permit) = Arc::clone(&gate).acquire_owned().await else {
                continue;
            };
            let client = client.clone();
            let found = Arc::clone(&found);
            let host = host.clone();
            tasks.spawn(async move {
                // Unreachable host/port: ignore.
                if let Some(server) = probe(&client, &host, port).await {
                    let mut found = found.lock();
                    if found.1.insert(server.url.to_lowercase()) {
                        found.0.push(server);
                    }
                }
                drop(permit);
            });
        }
    }
    while tasks.join_next().await.is_some() {}
    let mut servers = std::mem::take(&mut found.lock().0);
    servers.sort_by_key(|s| s.url.to_lowercase());
    servers
}

/// Ping-probe one host:port. Returns a server only if it answers with a Subsonic envelope.
async fn probe(client: &reqwest::Client, ip: &str, port: u16) -> Option<DiscoveredServer> {
    let base_url = format!("http://{ip}:{port}");
    let url = format!("{base_url}/rest/ping.view?c=octo&v=1.16.1&f=json");
    let response = client.get(&url).send().await.ok()?;
    let body = response.text().await.ok()?;
    if !body.to_lowercase().contains("subsonic-response") {
        return None;
    }

    let (mut kind, mut version, mut requires_auth) = (None, None, false);
    // Non-JSON but contained the marker: still count it. A value of the wrong kind stopped the
    // reading where it was, as GetString threw.
    if let Ok(document) = serde_json::from_str::<serde_json::Value>(&body)
        && let Some(r) = document.get("subsonic-response")
    {
        let _ = (|| -> Option<()> {
            if let Some(t) = r.get("type") {
                kind = string_or_stop(t)?;
            }
            if let Some(v) = r.get("serverVersion") {
                version = string_or_stop(v)?;
            }
            // A "failed / code 40" ping is still a positive server hit — it just means it
            // wants credentials, which the user supplies later.
            if let Some(s) = r.get("status") {
                requires_auth = string_or_stop(s)?.as_deref() == Some("failed");
            }
            Some(())
        })();
    }
    Some(DiscoveredServer {
        url: base_url,
        kind,
        server_version: version,
        requires_auth,
    })
}

/// `GetString()`: a string or null, and `None` (stop) for anything else.
fn string_or_stop(value: &serde_json::Value) -> Option<Option<String>> {
    match value {
        serde_json::Value::String(s) => Some(Some(s.clone())),
        serde_json::Value::Null => Some(None),
        _ => None,
    }
}

/// The host's own /24 address list (network+1 .. network+254), across every up, non-loopback
/// IPv4 interface. Capped at /24 so the sweep is bounded even when the real subnet is larger.
fn build_targets() -> Vec<String> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut ips: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for interface in interfaces {
        if !interface.is_oper_up() || interface.is_loopback() {
            continue;
        }
        let IpAddr::V4(address) = interface.ip() else {
            continue;
        };
        let [a, b, c, _] = address.octets();
        // Skip link-local 169.254.x.x and anything that isn't a normal LAN.
        if a == 169 && b == 254 {
            continue;
        }
        // Enumerate the /24 containing this address (a.b.c.x, 1..254).
        for host in 1..=254 {
            let ip = format!("{a}.{b}.{c}.{host}");
            if seen.insert(ip.clone()) {
                ips.push(ip);
            }
        }
    }
    ips
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_subsonic_envelope_is_a_server_even_when_it_wants_credentials() {
        let navidrome = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/ping.view"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","error":{"code":40}}}"#,
            ))
            .mount(&navidrome)
            .await;
        let other = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("<html>nope</html>"))
            .mount(&other)
            .await;

        let ports: Vec<u16> = [&navidrome, &other].iter().map(|s| s.address().port()).collect();
        let found = probe_all(&["127.0.0.1".to_string()], &ports).await;

        assert_eq!(
            found,
            [DiscoveredServer {
                url: format!("http://127.0.0.1:{}", ports[0]),
                kind: Some("navidrome".into()),
                server_version: Some("0.64.2".into()),
                requires_auth: true,
            }]
        );
    }

    #[tokio::test]
    async fn a_marker_in_something_that_is_not_json_still_counts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"<SUBSONIC-RESPONSE status="ok"/>"#))
            .mount(&server)
            .await;
        let found = probe_all(&["127.0.0.1".to_string()], &[server.address().port()]).await;
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].kind.clone(), found[0].requires_auth), (None, false));
    }

    #[test]
    fn the_sweep_skips_loopback() {
        assert!(build_targets().iter().all(|ip| !ip.starts_with("127.")));
    }
}
