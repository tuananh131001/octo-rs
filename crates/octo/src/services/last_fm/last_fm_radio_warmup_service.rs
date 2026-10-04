//! Port of `Services/LastFm/LastFmRadioWarmupService.cs`, singleton and hosted: the one instance
//! is the "LastFmRadioWarmupService" worker and the queue the refresh worker feeds.
//!
//! Keeps persisted Radio snapshots ready independently of client traffic. Work stays inside core
//! Octo and uses the temporary Radio cache; it never records a play, scrobbles, learns, or
//! acquires a permanent library copy.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::OperationCanceled;
use super::last_fm_radio_state_store::LastFmRadioStateStore;
use super::last_fm_radio_stream_service::{LastFmRadioStreamService, RadioWarmupResult};

const READINESS_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const CAPACITY: usize = 100;

pub struct LastFmRadioWarmupService {
    sender: mpsc::Sender<String>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<String>>,
    /// The users waiting (`StringComparer.OrdinalIgnoreCase`).
    queued: Mutex<HashSet<String>>,
    /// The service each warm resolves a scope of.
    streams: LastFmRadioStreamService,
    state: Arc<LastFmRadioStateStore>,
}

impl LastFmRadioWarmupService {
    pub fn new(streams: LastFmRadioStreamService, state: Arc<LastFmRadioStateStore>) -> Self {
        let (sender, receiver) = mpsc::channel(CAPACITY);
        LastFmRadioWarmupService {
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            queued: Mutex::new(HashSet::new()),
            streams,
            state,
        }
    }

    /// Queues a warm of the listener's stations unless one is already waiting or the queue is
    /// full.
    pub fn queue_user(&self, username: &str) -> bool {
        let username = username.trim();
        let key = dotnet::ordinal_ignore_case_key(username);
        if username.is_empty() || !self.queued.lock().insert(key.clone()) {
            return false;
        }
        if self.sender.try_send(username.to_string()).is_ok() {
            return true;
        }
        self.queued.lock().remove(&key);
        false
    }

    pub async fn process(
        &self,
        username: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<RadioWarmupResult> {
        let streams = self.streams.new_scope();
        streams.warm_stored_stations(username, cancellation_token).await
    }

    /// `ExecuteAsync`: every persisted profile is warmed at startup, and again whenever the
    /// minute scan finds it.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let known_users = self.state.known_users();
        info!(
            "Radio startup warm found {} persisted Radio profiles",
            known_users.len()
        );
        for username in &known_users {
            self.queue_user(username);
        }
        let scan = tokio::spawn(Arc::clone(&self).scan_readiness(stopping.clone()));
        loop {
            let username = {
                let mut receiver = self.receiver.lock().await;
                tokio::select! {
                    biased;
                    () = stopping.cancelled() => None,
                    username = receiver.recv() => username,
                }
            };
            let Some(username) = username else { break };
            match self.process(&username, &stopping).await {
                Ok(result) if result.ready_station_count == result.station_count => info!(
                    "Radio cache warm for {username}: {}/{} stations, {} ready tracks",
                    result.ready_station_count, result.station_count, result.ready_track_count
                ),
                Ok(result) => warn!(
                    "Radio cache warm incomplete for {username}: {}/{} stations; will retry",
                    result.ready_station_count, result.station_count
                ),
                Err(error) if stopping.is_cancelled() && error.is::<OperationCanceled>() => {
                    self.queued
                        .lock()
                        .remove(&dotnet::ordinal_ignore_case_key(&username));
                    break;
                }
                // Startup dependencies such as the yt-dlp shim may still be coming online. The
                // minute scan retries without poisoning station state.
                Err(error) => warn!("Radio cache warm failed for {username}; will retry: {error:#}"),
            }
            self.queued
                .lock()
                .remove(&dotnet::ordinal_ignore_case_key(&username));
        }
        let _ = scan.await;
        Ok(())
    }

    async fn scan_readiness(self: Arc<Self>, stopping: CancellationToken) {
        let mut timer = tokio::time::interval_at(
            tokio::time::Instant::now() + READINESS_SCAN_INTERVAL,
            READINESS_SCAN_INTERVAL,
        );
        // A PeriodicTimer tick that came while the scan was busy is not queued up.
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = stopping.cancelled() => return,
                _ = timer.tick() => {}
            }
            for username in self.state.known_users() {
                self.queue_user(&username);
            }
        }
    }
}
