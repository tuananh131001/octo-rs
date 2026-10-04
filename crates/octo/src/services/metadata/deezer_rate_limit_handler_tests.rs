//! Port of `octo.Tests/DeezerRateLimitHandlerTests.cs`.
//!
//! The existing Deezer harness builds the service over a mock server whose host is not the
//! API's, so it runs WITHOUT the metering. These drive the handler directly, because the traps
//! here are all silent: a wrongly-metered CDN, a lane that steals another lane's budget, or a
//! rejection that never surfaces.
//!
//! The mock server stands in for every host: requests to `127.0.0.1` are the API (the handler
//! is told that is the API host), and requests to `localhost`, the same server under another
//! name, are the CDN and unrelated hosts.

use futures::FutureExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// Terminates the chain and records what actually reached the network.
async fn build() -> (DeezerRateLimitHandler, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let handler = DeezerRateLimitHandler::with_api_host(Arc::new(DeezerRateLimiter::new()), "127.0.0.1");
    (handler, server)
}

fn other_host(server: &MockServer, path: &str) -> String {
    format!("http://localhost:{}{path}", server.address().port())
}

async fn calls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .map_or(0, |requests| requests.len())
}

/// The cover-art lookup fetches image bytes from the CDN through the SAME client it
/// uses for API calls. Metering those would spend an API permit per rendered row and
/// throttle a host that has no quota, so the budget must ignore them entirely.
#[tokio::test]
async fn cdn_requests_are_not_metered() {
    let (handler, server) = build().await;

    // Far more than the interactive permit count, so metering these would visibly
    // throttle them.
    for i in 0..120 {
        let answer = handler
            .get(
                &other_host(&server, &format!("/images/cover/{i}/1000x1000.jpg")),
                false,
                None,
            )
            .await
            .expect("the CDN answers");
        assert_eq!(answer.status, StatusCode::OK);
    }
    assert_eq!(calls(&server).await, 120);

    // Asserting they merely SUCCEEDED is not enough: a metered burst still succeeds,
    // it just waits for permits. What proves they were unmetered is that the whole
    // interactive window is still intact afterwards, with every permit available
    // without waiting.
    for i in 0..30 {
        let lease = handler.limiter().acquire(false).now_or_never();
        let lease = lease.unwrap_or_else(|| {
            panic!("interactive permit {i} had to wait, so CDN traffic spent the API budget")
        });
        assert!(lease.is_acquired());
    }
}

/// Anything that is not the Deezer API is passed straight through.
#[tokio::test]
async fn unrelated_hosts_are_passed_through() {
    let (handler, server) = build().await;

    let answer = handler
        .get(&other_host(&server, "/search?term=x"), false, None)
        .await
        .expect("answers");

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(calls(&server).await, 1);
}

/// API calls really do spend the budget, and it is bounded. Note this is a sliding
/// WINDOW: permits come back with time, not when a lease is disposed, so an
/// over-budget caller waits rather than being refused outright.
#[tokio::test]
async fn api_requests_consume_the_interactive_budget() {
    let (handler, server) = build().await;

    for i in 0..30 {
        let answer = handler
            .get(&format!("{}/album/{i}", server.uri()), false, None)
            .await
            .expect("answers");
        assert_eq!(answer.status, StatusCode::OK);
    }
    assert_eq!(calls(&server).await, 30);

    // The window is spent, so the next acquire must queue rather than complete now. From here
    // on nothing touches the network, so the clock can be paused and the wait skipped.
    tokio::time::pause();
    let queued = handler.limiter().acquire(false);
    tokio::pin!(queued);
    assert!(futures::poll!(&mut queued).is_pending());

    let lease = tokio::time::timeout(Duration::from_secs(10), queued)
        .await
        .expect("a permit came back within the window");
    assert!(lease.is_acquired());
}

/// Background cache warming must not be able to starve a search the user is waiting
/// on. Two limiters whose permits summed above the ceiling would defeat the point, so
/// this pins that they are genuinely separate allowances.
#[tokio::test]
async fn background_lane_does_not_consume_interactive_permits() {
    let limiter = DeezerRateLimiter::new();

    for _ in 0..10 {
        assert!(limiter.acquire(true).await.is_acquired());
    }

    // Background is spent; interactive must still be immediately available.
    let interactive = limiter.acquire(false).now_or_never().expect("no wait");
    assert!(interactive.is_acquired());
    // And the background lane really is spent.
    assert!(limiter.acquire(true).now_or_never().is_none());
}

/// Rust-only: a full queue answers 429 at once instead of waiting, and the caller sees it as
/// an ordinary refusal.
#[tokio::test]
async fn a_full_queue_answers_429() {
    let (handler, server) = build().await;
    for _ in 0..10 {
        assert!(handler.limiter().acquire(true).await.is_acquired());
    }
    // Fill the background queue (32) with waiters that are never polled to completion.
    let mut waiting = Vec::new();
    for _ in 0..32 {
        let mut waiter = Box::pin(handler.limiter().acquire(true));
        assert!(futures::poll!(&mut waiter).is_pending());
        waiting.push(waiter);
    }

    let answer = handler
        .get(&format!("{}/album/1", server.uri()), true, None)
        .await
        .expect("answers");
    assert_eq!(answer.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(calls(&server).await, 0);
    drop(waiting);
}
