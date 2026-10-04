//! LyricsTests (LRCLIB), the LRCLIB parts of LyricsWordTimingTests, SongIdentityTests and
//! QueryVariantLookupTests.

use octo_core::lyrics::{ILyricsSource, LyricsQuery, LyricsTiming};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

use super::LrclibLyricsSource;
use crate::services::lyrics::lyrics_http::lyrics_http_client;
use crate::services::lyrics::test_support::{FakeHttp, json, status};

fn source(http: &FakeHttp) -> LrclibLyricsSource {
    LrclibLyricsSource::with_base_url(lyrics_http_client(), &http.uri())
}

fn none() -> CancellationToken {
    CancellationToken::new()
}

// LyricsTests.Lrclib_Found_ReadsSyncedAndPlain
#[tokio::test]
async fn lrclib_found_reads_synced_and_plain() {
    let http = FakeHttp::start(|_| {
        json(
            r#"{"trackName":"Teardrop","artistName":"Massive Attack","duration":331.0,"instrumental":false,"syncedLyrics":"[01:02.38] Love, love is a verb","plainLyrics":"Love, love is a verb"}"#,
        )
    })
    .await;

    let lookup = source(&http)
        .find(
            &LyricsQuery::new("Massive Attack", "Teardrop", Some("Mezzanine".into()), Some(331)),
            &none(),
        )
        .await;

    assert!(!lookup.transient);
    let result = lookup.result.expect("found");
    assert!(result.has_synced());
    assert_eq!(result.source, "LRCLIB");
}

// LyricsTests.Lrclib_404_FallsBackToSearchWithinTwoSeconds
#[tokio::test]
async fn lrclib_404_falls_back_to_search_within_two_seconds() {
    let http = FakeHttp::start(|url| {
        if url.contains("/api/get?") {
            status(404, r#"{"code":404}"#)
        } else {
            json(
                r#"[{"trackName":"Teardrop","artistName":"Massive Attack","duration":400,"plainLyrics":"wrong length"},
                    {"trackName":"Teardrop","artistName":"Massive Attack","duration":331.5,"plainLyrics":"right one"}]"#,
            )
        }
    })
    .await;

    let started = std::time::Instant::now();
    let lookup = source(&http)
        .find(
            &LyricsQuery::new("Massive Attack", "Teardrop", None, Some(331)),
            &none(),
        )
        .await;

    assert_eq!(lookup.result.and_then(|r| r.plain).as_deref(), Some("right one"));
    assert_eq!(http.calls().await.len(), 2);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

/// A 503 is "not now", never "no lyrics", and LRCLIB asks callers to honour Retry-After.
// LyricsTests.Lrclib_Overloaded_CoolsDownWithoutAnotherRequest
#[tokio::test]
async fn lrclib_overloaded_cools_down_without_another_request() {
    let http = FakeHttp::start(|_| {
        status(503, r#"{"message":"The server is busy"}"#).insert_header("Retry-After", "300")
    })
    .await;
    let lrclib = source(&http);

    let first = lrclib
        .find(&LyricsQuery::new("A", "T", None, None), &none())
        .await;
    let second = lrclib
        .find(&LyricsQuery::new("B", "U", None, None), &none())
        .await;

    assert!(first.transient);
    assert!(second.transient);
    assert_eq!(http.calls().await.len(), 1);
    let until = lrclib.cool_down_until().expect("cooling down");
    assert!(until > tokio::time::Instant::now() + std::time::Duration::from_secs(290));
}

// LyricsTests.Lrclib_AlbumThatIsOnlyTheTitle_IsNotSent
#[tokio::test]
async fn lrclib_album_that_is_only_the_title_is_not_sent() {
    let http = FakeHttp::start(|_| status(404, "{}")).await;

    source(&http)
        .find(
            &LyricsQuery::new("A", "Single", Some("Single".into()), Some(200)),
            &none(),
        )
        .await;

    let calls = http.calls().await;
    assert!(!calls[0].contains("album_name"), "{}", calls[0]);
    assert!(calls[0].contains("duration=200"), "{}", calls[0]);
}

#[tokio::test]
async fn lrclib_sends_a_real_album_and_no_length_past_an_hour() {
    let http = FakeHttp::start(|_| status(404, "{}")).await;

    source(&http)
        .find(
            &LyricsQuery::new("A", "Song", Some("The Album".into()), Some(3601)),
            &none(),
        )
        .await;

    let calls = http.calls().await;
    assert!(
        calls[0].ends_with("/api/get?track_name=Song&artist_name=A&album_name=The Album"),
        "{}",
        calls[0]
    );
}

// LyricsWordTimingTests.Lrclib_HasWordSync_UsesTheLyricsfileWords (its LrclibLyricsSource.Parse half)
#[test]
fn lrclib_has_word_sync_uses_the_lyricsfile_words() {
    let row: Value = serde_json::from_str(
        r#"{"id":7,"trackName":"Song","artistName":"Someone","duration":200,"hasWordSync":true,
             "syncedLyrics":"[00:01.00]Hello me",
             "lyricsfile":"version: '1.0'\nlines:\n  - text: 'Hello me'\n    start_ms: 1000\n    words:\n      - text: 'Hello '\n        start_ms: 1000\n      - text: 'me'\n        start_ms: 1500\n        end_ms: 1900\n"}"#,
    )
    .expect("json");

    let result = LrclibLyricsSource::parse(&row, None).expect("parsed");

    assert_eq!(result.timing(), LyricsTiming::Word);
    assert_eq!(result.candidate_id.as_deref(), Some("lrclib:7"));
    assert_eq!(result.doubt, None);
}

// SongIdentityTests.Lyrics_Lrclib_NeverTakesAnotherVersion (7 rows)
#[test]
fn lyrics_lrclib_never_takes_another_version() {
    for (want, got) in super::super::kugou_lyrics_source::tests::OTHER_VERSIONS {
        let row = serde_json::json!({ "trackName": got, "artistName": "Artist", "duration": 200 });
        assert!(
            !LrclibLyricsSource::is_this_song(&row, &LyricsQuery::new("Artist", want, None, Some(200)))
                .expect("an object"),
            "{want} / {got}"
        );
    }
}

// QueryVariantLookupTests.Lrclib_SearchesAgainUnderTheSpelledOutName
#[tokio::test]
async fn lrclib_searches_again_under_the_spelled_out_name() {
    let http = FakeHttp::start(|url| {
        if url.contains("track_name=SUICIDE&artist_name=suicideboys") {
            json(
                r#"[{"id":42,"trackName":"Suicide","artistName":"Suicideboys","duration":170.0,"syncedLyrics":"[00:01.00]Line one"}]"#,
            )
        } else if url.contains("/api/search") {
            json("[]")
        } else {
            status(404, "{}")
        }
    })
    .await;

    let lookup = source(&http)
        .find(
            &LyricsQuery::new("$uicideboy$", "$UICIDE", None, Some(170)),
            &none(),
        )
        .await;

    assert_eq!(
        lookup.result.and_then(|r| r.candidate_id).as_deref(),
        Some("lrclib:42")
    );
    let calls = http.calls().await;
    assert_eq!(
        calls.iter().filter(|c| c.contains("/api/search")).count(),
        2,
        "{calls:?}"
    );
}

// QueryVariantLookupTests.Lrclib_APlainSong_CostsNoExtraSearch
#[tokio::test]
async fn lrclib_a_plain_song_costs_no_extra_search() {
    let http = FakeHttp::start(|url| {
        if url.contains("/api/search") {
            json("[]")
        } else {
            status(404, "{}")
        }
    })
    .await;

    let lookup = source(&http)
        .find(&LyricsQuery::new("Drake", "Landed", None, Some(200)), &none())
        .await;

    assert!(lookup.result.is_none());
    let calls = http.calls().await;
    assert_eq!(
        calls.iter().filter(|c| c.contains("/api/search")).count(),
        1,
        "{calls:?}"
    );
}

#[tokio::test]
async fn lrclib_search_offers_the_entries_with_their_lyrics_and_fetch_reads_one() {
    let http = FakeHttp::start(|url| {
        if url.contains("/api/search") {
            json(
                r#"[{"id":5,"trackName":"Song","artistName":"Someone","albumName":"LP","duration":200.5,"plainLyrics":"words"},
                    {"id":"x","trackName":"No id"},
                    {"id":6,"name":"Other","artistName":"Someone","duration":180.0,"syncedLyrics":"[00:01.00]x"}]"#,
            )
        } else if url.ends_with("/api/get/6") {
            json(r#"{"id":6,"trackName":"Other","syncedLyrics":"[00:01.00]x"}"#)
        } else {
            status(404, "{}")
        }
    })
    .await;
    let lrclib = source(&http);

    let search = lrclib
        .search(&LyricsQuery::new("Someone", "Song", None, None), &none())
        .await;
    assert!(!search.transient);
    let ids: Vec<String> = search.candidates.iter().map(|c| c.candidate_id()).collect();
    assert_eq!(ids, ["lrclib:5", "lrclib:6"]);
    // Math.Round(200.5) is 200, to even.
    assert_eq!(search.candidates[0].duration_seconds, Some(200));
    assert_eq!(search.candidates[0].album.as_deref(), Some("LP"));
    assert_eq!(search.candidates[1].title, "Other");
    assert_eq!(
        search.candidates[0]
            .lyrics
            .as_ref()
            .and_then(|l| l.plain.as_deref()),
        Some("words")
    );

    let fetched = lrclib.fetch(" 6", &none()).await;
    assert_eq!(
        fetched.result.and_then(|r| r.candidate_id).as_deref(),
        Some("lrclib:6")
    );
    assert_eq!(
        lrclib.fetch("nope", &none()).await,
        octo_core::lyrics::LyricsLookup::miss()
    );
    assert_eq!(
        lrclib.fetch("7", &none()).await,
        octo_core::lyrics::LyricsLookup::miss()
    );
}

#[tokio::test]
async fn lrclib_an_answer_that_is_not_json_or_not_an_object_is_not_now() {
    let http = FakeHttp::start(|url| {
        if url.contains("/api/get?") {
            json("[1,2]")
        } else {
            ResponseTemplate::new(200).set_body_string("<html>")
        }
    })
    .await;
    let lrclib = source(&http);

    // The array where an object belongs threw in C#, and the service read that as a failure.
    assert!(
        lrclib
            .find(&LyricsQuery::new("A", "B", None, None), &none())
            .await
            .transient
    );
    assert!(
        lrclib
            .search(&LyricsQuery::new("A", "B", None, None), &none())
            .await
            .transient
    );
}

#[tokio::test]
async fn lrclib_a_cancelled_lookup_is_not_now() {
    let http = FakeHttp::start(|_| status(404, "{}")).await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    let lookup = source(&http)
        .find(&LyricsQuery::new("A", "B", None, None), &cancelled)
        .await;

    assert!(lookup.transient);
    assert!(http.calls().await.is_empty());
}
