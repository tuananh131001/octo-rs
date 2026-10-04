//! Port of the client half of `octo.Tests/MusicBrainzReleaseDetailsTests.cs` ("the client:
//! cache and escaping"). Its `ReleaseDetails.Parse` and `CandidateSources` tests belong to the
//! tagging port (2-A); its `MusicBrainzQueryTests` are in
//! `octo_core::fingerprint::music_brainz_client`.

use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const NIGHT_AT_THE_OPERA: &str = r#"
{
  "id": "r-nato", "title": "A Night at the Opera", "status": "Official", "date": "1975-11-21", "country": "GB",
  "barcode": "077778949224", "quality": "normal",
  "label-info": [{"catalog-number": "EMTC 103", "label": {"id": "l-emi", "name": "EMI"}}],
  "release-group": {"id": "g-nato", "title": "A Night at the Opera", "primary-type": "Album",
    "secondary-types": [], "first-release-date": "1975-11-21"},
  "artist-credit": [{"name": "Queen", "joinphrase": "", "artist": {"id": "a-queen", "name": "Queen"}}],
  "media": [{"position": 1, "format": "12\" Vinyl", "track-count": 12,
    "tracks": [
      {"id": "t-1", "position": 1, "number": "A1", "title": "Death on Two Legs", "length": 223000,
       "recording": {"id": "rec-dotl", "title": "Death on Two Legs (Dedicated to...)", "length": 223000, "isrcs": ["GBUM71029604"]}},
      {"id": "t-11", "position": 11, "number": "B5", "title": "Bohemian Rhapsody", "length": 355000,
       "recording": {"id": "rec-br", "title": "Bohemian Rhapsody", "length": 355000, "isrcs": ["GBUM71029604", "GBUM71029605"]}}
    ]}],
  "genres": [{"name": "rock", "count": 12}, {"name": "progressive rock", "count": 5}, {"name": "glam rock", "count": 1}]
}
"#;

async fn client(answer: ResponseTemplate) -> (MusicBrainzClient, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(answer).mount(&server).await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    (MusicBrainzClient::with_base_url(base), server)
}

async fn calls(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.to_string())
        .collect()
}

#[tokio::test]
async fn lookup_release_asks_once_then_answers_from_memory() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(NIGHT_AT_THE_OPERA)).await;

    let first = client
        .lookup_release("r-nato")
        .await
        .expect("no timeout")
        .expect("found");
    let second = client
        .lookup_release("r-nato")
        .await
        .expect("no timeout")
        .expect("found");

    assert!(Arc::ptr_eq(&first, &second));
    let calls = calls(&server).await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(
        "release/r-nato?inc=labels+release-groups+artist-credits+recordings+isrcs+genres&fmt=json"
    ));
    assert!(calls[0].contains("/ws/2/release/"));
    // MusicBrainz wants a User-Agent naming the application.
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests[0]
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok()),
        Some(octo_user_agent::value())
    );
}

#[tokio::test]
async fn lookup_release_server_error_is_null_and_not_remembered() {
    let (client, server) = client(ResponseTemplate::new(503)).await;

    assert!(
        client
            .lookup_release("r-nato")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(
        client
            .lookup_release("r-nato")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert_eq!(calls(&server).await.len(), 2);
}

#[tokio::test]
async fn search_recordings_is_remembered_for_the_same_song() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(r#"{"recordings": []}"#)).await;

    let first = client
        .search_recordings("Portishead", "Glory Box (Live)", 300)
        .await
        .expect("no timeout");
    let second = client
        .search_recordings("Portishead", "Glory Box (Live)", 300)
        .await
        .expect("no timeout");

    assert!(first.is_some());
    assert!(second.is_some());
    assert_eq!(calls(&server).await.len(), 1);
}

/// Rust-only: the second request waits out the one-a-second gap.
#[tokio::test]
async fn requests_are_spaced_a_second_apart() {
    let (client, _server) =
        client(ResponseTemplate::new(200).set_body_string(r#"{"isrcs": ["GBUM71029604"]}"#)).await;

    let start = Instant::now();
    assert_eq!(
        client.fetch_isrcs("rec-1").await.expect("no timeout"),
        Some(vec!["GBUM71029604".to_string()])
    );
    client.fetch_isrcs("rec-2").await.expect("no timeout");
    assert!(Instant::now() - start >= Duration::from_millis(1100));
}
