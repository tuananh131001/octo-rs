//! Port of `Services/Soulseek/SoulseekLink.cs`: the cached read of slskd's login and the wait for
//! it. The reading, its state and the dashboard's words are `octo_core::soulseek::soulseek_link`.
//!
//! The C# methods took a `CancellationToken`; here a caller that gives up drops the future.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::settings::SettingsStore;
use octo_core::soulseek::soulseek_client::delta;
use tracing::{info, warn};

pub use octo_core::soulseek::soulseek_link::{
    CACHE_FOR, OFFLINE_TEXT, POLL_EVERY, SoulseekLinkState, SoulseekServerReading, describe, outage_detail,
};

use super::soulseek_client::SoulseekClient;

/// Whether slskd is logged in to Soulseek, for the parts of Octo that should wait for it rather
/// than settle for a lossy copy. An interface so the heart chain and the workers can be tested
/// against a scripted outage.
#[async_trait]
pub trait ISoulseekLink: Send + Sync {
    /// None when slskd did not answer at all. `fresh` skips the short cache.
    async fn read(&self, fresh: bool) -> Option<SoulseekServerReading>;

    /// How long a Soulseek-first download waits for slskd to log back in. Zero is off.
    fn hold_limit(&self) -> TimeDelta;

    fn utc_now(&self) -> DateTime<Utc>;

    /// Returns once slskd is logged in, cannot say, or `deadline_utc` has passed.
    /// True when Soulseek is worth trying; false when the wait ran out with slskd still out.
    async fn wait_for_login(&self, deadline_utc: DateTime<Utc>) -> bool;
}

type ReadFn = Arc<dyn Fn() -> BoxFuture<'static, Option<SoulseekServerReading>> + Send + Sync>;
type DelayFn = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// The last reading and when it was taken, behind the read lock.
struct Cached {
    last: Option<SoulseekServerReading>,
    last_at: DateTime<Utc>,
    last_logged: Option<SoulseekLinkState>,
}

pub struct SoulseekLink {
    /// `IOptionsMonitor<SoulseekSettings>`: the hold is read live.
    settings: Arc<SettingsStore>,
    // Seams, the same way SoulseekClient exposes Clock and PollInterval.
    read: ReadFn,
    clock: Clock,
    delay: DelayFn,
    // The C# `SemaphoreSlim _readLock`: held across the read, so a burst shares one request.
    cached: tokio::sync::Mutex<Cached>,
}

impl SoulseekLink {
    pub fn new(client: SoulseekClient, settings: Arc<SettingsStore>) -> Self {
        let read: ReadFn = Arc::new(move || {
            let client = client.clone();
            async move { client.read_server().await }.boxed()
        });
        SoulseekLink {
            settings,
            read,
            clock: Clock::system(),
            delay: Arc::new(|span| tokio::time::sleep(span).boxed()),
            cached: tokio::sync::Mutex::new(Cached {
                last: None,
                last_at: DateTime::<Utc>::MIN_UTC,
                last_logged: None,
            }),
        }
    }

    /// The read seam: what one look at slskd answers.
    pub fn with_read<F, Fut>(mut self, read: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Option<SoulseekServerReading>> + Send + 'static,
    {
        self.read = Arc::new(move || read().boxed());
        self
    }

    /// The clock seam.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The delay seam.
    pub fn with_delay<F, Fut>(mut self, delay: F) -> Self
    where
        F: Fn(Duration) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.delay = Arc::new(move |span| delay(span).boxed());
        self
    }

    fn hold_hours(&self) -> i32 {
        self.settings.current().soulseek.effective_outage_hold_hours()
    }

    // Once per change, so a three hour outage is two log lines, not one per held song.
    fn note_change(&self, cached: &mut Cached, reading: Option<&SoulseekServerReading>) {
        let Some(now) = reading.map(|r| r.link) else {
            return;
        };
        if now == SoulseekLinkState::Unknown || Some(now) == cached.last_logged {
            return;
        }
        if now == SoulseekLinkState::NotLoggedIn {
            warn!(
                "slskd is not logged in to Soulseek ({}); Soulseek-first downloads wait up to {} h",
                reading.and_then(|r| r.state.as_deref()).unwrap_or("no state"),
                self.hold_hours()
            );
        } else if cached.last_logged.is_some() {
            info!("slskd is logged in to Soulseek again");
        }
        cached.last_logged = Some(now);
    }
}

#[async_trait]
impl ISoulseekLink for SoulseekLink {
    async fn read(&self, fresh: bool) -> Option<SoulseekServerReading> {
        let mut cached = self.cached.lock().await;
        if !fresh && self.clock.now() - cached.last_at < delta(CACHE_FOR) {
            return cached.last.clone();
        }
        let reading = (self.read)().await;
        cached.last = reading.clone();
        cached.last_at = self.clock.now();
        self.note_change(&mut cached, reading.as_ref());
        reading
    }

    fn hold_limit(&self) -> TimeDelta {
        TimeDelta::hours(i64::from(self.hold_hours()))
    }

    fn utc_now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    async fn wait_for_login(&self, deadline_utc: DateTime<Utc>) -> bool {
        loop {
            let reading = self.read(false).await;
            if reading.map(|r| r.link) != Some(SoulseekLinkState::NotLoggedIn) {
                return true;
            }
            let left = deadline_utc - self.clock.now();
            if left <= TimeDelta::zero() {
                return false;
            }
            let wait = left.to_std().unwrap_or(Duration::ZERO).min(POLL_EVERY);
            (self.delay)(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The link half of `octo.Tests/SoulseekOutageTests.cs` (the wait and the cache). The heart
    //! chain half belongs to 4-D (HeartAcquisitionCoordinator).

    use chrono::TimeZone;
    use octo_core::settings::{AppSettings, SoulseekSettings};
    use parking_lot::Mutex;

    use super::*;

    fn store(settings: SoulseekSettings) -> Arc<SettingsStore> {
        Arc::new(SettingsStore::for_tests(AppSettings {
            soulseek: settings,
            ..Default::default()
        }))
    }

    /// A link over a script of states (the last repeats), whose delays move its own clock.
    fn scripted_link(script: Vec<Option<SoulseekLinkState>>) -> (SoulseekLink, Arc<Mutex<Vec<Duration>>>) {
        let now = Arc::new(Mutex::new(
            Utc.with_ymd_and_hms(2026, 10, 3, 3, 0, 0)
                .single()
                .expect("a date"),
        ));
        let delays = Arc::new(Mutex::new(Vec::new()));
        let reads = Arc::new(Mutex::new(0usize));
        let client = SoulseekClient::new(&SoulseekSettings::default());
        let link =
            SoulseekLink::new(client, store(SoulseekSettings::default()))
                .with_read(move || {
                    let mut n = reads.lock();
                    let state = script[(*n).min(script.len() - 1)];
                    *n += 1;
                    async move {
                        state.map(|s| SoulseekServerReading::new(s, Some(s.name().to_string()), None, None))
                    }
                })
                .with_clock({
                    let now = now.clone();
                    Clock::new(move || *now.lock())
                })
                .with_delay({
                    let delays = delays.clone();
                    move |span| {
                        delays.lock().push(span);
                        *now.lock() += TimeDelta::from_std(span).expect("a short span");
                        async {}
                    }
                });
        (link, delays)
    }

    #[tokio::test]
    async fn the_wait_ends_when_slskd_logs_back_in() {
        let (link, delays) = scripted_link(vec![
            Some(SoulseekLinkState::NotLoggedIn),
            Some(SoulseekLinkState::NotLoggedIn),
            Some(SoulseekLinkState::LoggedIn),
        ]);
        let back = link.wait_for_login(link.utc_now() + TimeDelta::hours(6)).await;
        assert!(back);
        assert_eq!(*delays.lock(), [Duration::from_secs(30), Duration::from_secs(30)]);
    }

    #[tokio::test]
    async fn the_wait_runs_out_at_the_deadline() {
        let (link, delays) = scripted_link(vec![Some(SoulseekLinkState::NotLoggedIn)]);
        let back = link.wait_for_login(link.utc_now() + TimeDelta::seconds(70)).await;
        assert!(!back);
        assert_eq!(
            *delays.lock(),
            [
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(10)
            ]
        );
    }

    #[tokio::test]
    async fn an_slskd_that_does_not_answer_is_never_waited_for() {
        let (link, delays) = scripted_link(vec![None]);
        assert!(link.wait_for_login(link.utc_now() + TimeDelta::hours(6)).await);
        assert!(delays.lock().is_empty());
    }

    #[tokio::test]
    async fn reads_within_ten_seconds_share_one_request() {
        let now = Arc::new(Mutex::new(Utc::now()));
        let calls = Arc::new(Mutex::new(0));
        let link = SoulseekLink::new(
            SoulseekClient::new(&SoulseekSettings::default()),
            store(SoulseekSettings::default()),
        )
        .with_read({
            let calls = calls.clone();
            move || {
                *calls.lock() += 1;
                async {
                    Some(SoulseekServerReading::new(
                        SoulseekLinkState::LoggedIn,
                        None,
                        None,
                        None,
                    ))
                }
            }
        })
        .with_clock({
            let now = now.clone();
            Clock::new(move || *now.lock())
        });
        link.read(false).await;
        *now.lock() += TimeDelta::seconds(9);
        link.read(false).await;
        assert_eq!(*calls.lock(), 1);
        link.read(true).await;
        assert_eq!(*calls.lock(), 2);
        *now.lock() += TimeDelta::seconds(11);
        link.read(false).await;
        assert_eq!(*calls.lock(), 3);
    }

    #[test]
    fn the_hold_limit_is_clamped_and_live() {
        let settings = store(SoulseekSettings {
            outage_hold_hours: 99,
            ..Default::default()
        });
        let link = SoulseekLink::new(
            SoulseekClient::new(&SoulseekSettings::default()),
            settings.clone(),
        );
        assert_eq!(link.hold_limit(), TimeDelta::hours(48));
        settings.set(AppSettings {
            soulseek: SoulseekSettings {
                outage_hold_hours: 0,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(link.hold_limit(), TimeDelta::zero());
        assert_eq!(SoulseekSettings::default().outage_hold_hours, 6);
    }

    /// Not in the C#: a real link reads slskd's application state through the client.
    #[tokio::test]
    async fn a_real_link_reads_slskd_through_the_client() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/application"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"server":{"state":"Disconnecting","isConnected":false,"isLoggedIn":false}}"#,
            ))
            .mount(&server)
            .await;
        let client = SoulseekClient::new(&SoulseekSettings {
            base_url: Some(server.uri()),
            ..Default::default()
        });
        let link = SoulseekLink::new(client, store(SoulseekSettings::default()));
        let reading = link.read(true).await.expect("an answer");
        assert_eq!(reading.link, SoulseekLinkState::NotLoggedIn);
        assert!(describe(Some(&reading), 6).0);
        assert!(OFFLINE_TEXT.starts_with("Soulseek is not connected"));
        assert!(outage_detail(&reading, 0).ends_with("Downloads use the next source."));
    }
}
