//! `LibraryActionKeepTests.cs`: `SetRatingScopeTests` (setRating through the app) and
//! `PlaylistNames_ReadsXmlAsWellAsJson`.
//!
//! Where a star counts as a command (#47). With Review on, a star is a command only on a track
//! Octo asked the person about; everywhere else it is just a rating, relayed and left alone.

use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, LibraryRatingScope,
};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer};

use super::playlist_names_6a2::playlist_names;
use super::test_support_6a2::*;
use crate::services::library::library_action_test_support::asked_about;

/// Navidrome answering ok to everything.
async fn ok_navidrome() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome_json(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
        ))
        .mount(&server)
        .await;
    server
}

fn rating_settings(url: &str, scope: LibraryRatingScope) -> AppSettings {
    AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            ratings_enabled: true,
            review_enabled: true,
            ratings_scope: scope,
            allowed_users: vec!["alice".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::Delete,
                enabled: true,
                rating: Some(1),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..settings(url)
    }
}

fn rate(id: &str) -> String {
    format!("/rest/setRating.view?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json&id={id}&rating=1")
}

#[tokio::test]
async fn set_rating_with_review_on_ignores_a_track_octo_did_not_ask_about() {
    for scope in [LibraryRatingScope::NoticeOnly, LibraryRatingScope::Auto] {
        let navidrome = ok_navidrome().await;
        let state = state_with(rating_settings(&navidrome.uri(), scope), |inner| {
            inner.notice_queue = asked_about("alice", "nd-asked");
        });
        let app = app(&state);

        assert!(
            get(&app, &rate("nd-elsewhere")).await.status.is_success(),
            "{scope:?}"
        );
        assert_eq!(state.library_action_rating_worker.pending(), 0, "{scope:?}");

        assert!(
            get(&app, &rate("nd-asked")).await.status.is_success(),
            "{scope:?}"
        );
        assert_eq!(state.library_action_rating_worker.pending(), 1, "{scope:?}");
    }
}

#[tokio::test]
async fn set_rating_global_counts_on_any_track() {
    let navidrome = ok_navidrome().await;
    let state = state_with(
        rating_settings(&navidrome.uri(), LibraryRatingScope::Global),
        |inner| inner.notice_queue = asked_about("alice", "nd-asked"),
    );
    let app = app(&state);

    assert!(get(&app, &rate("nd-elsewhere")).await.status.is_success());

    assert_eq!(state.library_action_rating_worker.pending(), 1);
}

#[test]
fn playlist_names_reads_xml_as_well_as_json() {
    let xml = "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\"><playlists>\
        <playlist id=\"1\" name=\"\u{1F6E0} Delete\"/><playlist id=\"2\" name=\"\u{25B8} Review\"/>\
        </playlists></subsonic-response>";
    let json = "{\"subsonic-response\":{\"status\":\"ok\",\"playlists\":{\"playlist\":[{\"id\":\"1\",\"name\":\"\u{1F6E0} Delete\"}]}}}";

    assert_eq!(
        playlist_names(Some(xml.as_bytes()), "xml"),
        vec!["\u{1F6E0} Delete".to_string(), "\u{25B8} Review".to_string()]
    );
    assert_eq!(
        playlist_names(Some(json.as_bytes()), "json"),
        vec!["\u{1F6E0} Delete".to_string()]
    );
    assert!(playlist_names(Some(b"<not xml"), "xml").is_empty());
    // Rust-only: no body, and a JSON name that is not a string (GetValue threw), give nothing.
    assert!(playlist_names(None, "json").is_empty());
    assert!(
        playlist_names(
            Some(br#"{"subsonic-response":{"playlists":{"playlist":[{"name":"a"},{"name":3}]}}}"#),
            "JSON"
        )
        .is_empty()
    );
}
