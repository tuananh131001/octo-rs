//! The process: what `WebApplication.CreateBuilder` + `app.Run()` did. Logging, the settings
//! store and its watcher, the state, the listener, the workers, and a graceful shutdown on
//! SIGINT/SIGTERM (or `StopApplication()`) with the 10 s budget `HostOptions.ShutdownTimeout`
//! gave.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Context as _;
use octo_core::json::dom::Node;
use octo_core::settings::{JsonObject, SettingsStore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::app::AppState;
use crate::http::pipeline;
use crate::http::static_files::{StaticAssets, StaticRoots};
use crate::logging::{self, HOSTING_LIFETIME};
use crate::workers::SHUTDOWN_TIMEOUT;

/// Kestrel's address in the C# image (`ASPNETCORE_URLS=http://+:8080`).
pub const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);

/// The address to listen on from `ASPNETCORE_URLS`: its first `http://` entry, where `+`, `*`
/// and an empty host mean every interface and `localhost` the loopback; the port defaults to
/// 80 as Kestrel's did. Without a usable entry, `0.0.0.0:8080`.
pub fn bind_address(urls: Option<&str>) -> SocketAddr {
    let Some(urls) = urls else { return DEFAULT_BIND };
    for entry in urls.split(';').map(str::trim) {
        if entry.len() < 7 || !entry[..7].eq_ignore_ascii_case("http://") {
            continue;
        }
        let authority = entry[7..].split('/').next().unwrap_or("");
        if let Some(addr) = parse_authority(authority) {
            return addr;
        }
        eprintln!("Ignoring ASPNETCORE_URLS entry {entry}: not a host and port Octo can bind");
    }
    DEFAULT_BIND
}

fn parse_authority(authority: &str) -> Option<SocketAddr> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // [::1]:8080
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => 80,
            None => return None,
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), 80),
        }
    };
    let ip = match host.as_str() {
        "" | "+" | "*" | "0.0.0.0" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        h if h.eq_ignore_ascii_case("localhost") => IpAddr::V4(Ipv4Addr::LOCALHOST),
        // Kestrel binds any other host name to every interface (with a warning).
        h => h.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    };
    Some(SocketAddr::new(ip, port))
}

/// Resolves on SIGINT, SIGTERM or a cancelled `lifetime` (`StopApplication()`).
pub async fn shutdown_signal(lifetime: CancellationToken) {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Could not listen for Ctrl+C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                error!("Could not listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = lifetime.cancelled() => {}
    }
}

/// Runs Octo until it is told to stop.
pub async fn run() -> anyhow::Result<()> {
    logging::init();

    let settings_path = SettingsStore::default_settings_path();
    let store = SettingsStore::load(&settings_path);
    if let Err(e) = store.start_watching() {
        error!("Could not watch {} for changes: {e:#}", settings_path.display());
    }
    // RestartTracker is snapshotted here, before the dashboard or first-run automation can
    // change anything.
    let state = AppState::build(store);

    let roots = StaticRoots::from_env();
    let assets = tokio::task::spawn_blocking(move || {
        let assets = StaticAssets::load(&roots);
        (roots, assets)
    })
    .await
    .context("loading the static files")?;
    let (roots, assets) = assets;
    if assets.is_empty() {
        warn!(
            "No static files found (web root {:?}, assets {:?}); /admin will not load",
            roots.wwwroot, roots.assets
        );
    }
    let app = pipeline::build(state.clone(), &assets);
    // The Brotli and gzip variants, after the listener is up; plain bodies until then.
    let warming = assets.clone();
    tokio::task::spawn_blocking(move || warming.warm_all());

    // First-run automation, in the background while the host starts, as Program.cs ran it.
    tokio::spawn(first_run(state.clone()));

    // StartupValidationOrchestrator: a hosted service, whose StartAsync the host awaited before
    // the server started listening.
    state.startup_validation.start().await;

    let addr = bind_address(std::env::var("ASPNETCORE_URLS").ok().as_deref());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    info!(target: HOSTING_LIFETIME, "Now listening on: http://{addr}");

    state.workers.start();
    info!(target: HOSTING_LIFETIME, "Application started. Press Ctrl+C to shut down.");

    let lifetime = state.lifetime.clone();
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::ServiceExt::into_make_service(app))
            .with_graceful_shutdown(async move { server_stop.cancelled().await })
            .await
    });

    shutdown_signal(lifetime.clone()).await;
    info!(target: HOSTING_LIFETIME, "Application is shutting down...");
    lifetime.cancel();
    stop.cancel();

    // One budget for the in-flight requests and the workers together, as the host's
    // StopAsync had.
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    let server_done = async {
        match tokio::time::timeout_at(deadline, server).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => error!("The server failed while stopping: {e}"),
            Ok(Err(e)) => error!("The server task failed: {e}"),
            Err(_) => warn!("Requests still running after the shutdown timeout were dropped"),
        }
    };
    let (_, report) = tokio::join!(server_done, state.workers.shutdown_until(deadline));
    if !report.abandoned.is_empty() {
        warn!(
            "Abandoned background workers at shutdown: {}",
            report.abandoned.join(", ")
        );
    }
    Ok(())
}

/// First-run automation (best-effort, background). Octo is an accessory to an existing
/// Navidrome, so it self-configures what it can: if no upstream URL is set, scan the LAN and
/// adopt the server when exactly one is found; then detect the music folder from it. Anything
/// ambiguous (several servers, none found) is left for the dashboard so we never silently point
/// at the wrong server.
pub async fn first_run(state: AppState) {
    let url_missing = state
        .settings
        .current()
        .subsonic
        .url
        .as_deref()
        .is_none_or(|u| u.trim().is_empty());
    if url_missing {
        let servers = state.subsonic_discovery.scan().await;
        if servers.len() == 1 {
            let server = &servers[0];
            let mut section = JsonObject::new();
            section.insert("Url".to_string(), Node::String(server.url.clone()));
            let mut patch = JsonObject::new();
            patch.insert("Subsonic".to_string(), Node::Object(section));
            match state.settings_writer.merge(&patch, &[]) {
                Ok(_) => {
                    // The URL is a restart-required setting (services bind it at startup), so
                    // restart cleanly to apply it. A supervised deploy (compose restart policy /
                    // systemd) brings Octo straight back, now with the URL loaded; on next boot
                    // the URL is set so this is skipped.
                    info!(
                        target: "Program",
                        "First-run: auto-configured Navidrome URL -> {} ({} {}). Restarting to apply.",
                        server.url,
                        server.kind.as_deref().unwrap_or(""),
                        server.server_version.as_deref().unwrap_or("")
                    );
                    state.stop_application();
                    return;
                }
                Err(e) => warn!(target: "Program", "First-run server auto-detect failed: {e}"),
            }
        } else if servers.len() > 1 {
            info!(target: "Program", "First-run: {} servers found; pick one in the dashboard.", servers.len());
        } else {
            info!(target: "Program", "First-run: no Navidrome auto-detected; set the URL in the dashboard.");
        }
    }

    // Detect the music folder from whatever URL we now have (configured or adopted).
    state.navidrome_identity.detect_music_folder(true).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspnetcore_urls_pick_the_first_http_entry() {
        let cases: [(Option<&str>, &str); 10] = [
            (None, "0.0.0.0:8080"),
            (Some("http://+:8080"), "0.0.0.0:8080"),
            (Some("http://*:5000"), "0.0.0.0:5000"),
            (Some("http://127.0.0.1:18181"), "127.0.0.1:18181"),
            (Some("https://+:443;http://localhost:9000"), "127.0.0.1:9000"),
            (Some("HTTP://[::1]:7000/"), "[::1]:7000"),
            (Some("http://+"), "0.0.0.0:80"),
            (Some("http://0.0.0.0:1234/base"), "0.0.0.0:1234"),
            (Some("https://+:443"), "0.0.0.0:8080"),
            (Some("http://example.com:81"), "0.0.0.0:81"),
        ];
        for (urls, expected) in cases {
            assert_eq!(bind_address(urls).to_string(), expected, "{urls:?}");
        }
    }
}
