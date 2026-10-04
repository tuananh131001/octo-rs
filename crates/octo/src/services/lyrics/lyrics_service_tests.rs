//! LyricsTests (Service_*), LyricsSongSourceTests (Ranked_*) and LyricsWordTimingTests
//! (Order_*).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::lyrics::{ILyricsSource, LyricsLookup, LyricsQuery, LyricsResult, LyricsTiming};
use octo_core::settings::{AppSettings, MetadataSettings, SettingsStore};
use tokio_util::sync::CancellationToken;

use super::LyricsService;
use crate::services::lyrics::test_support::FakeSource;

fn service(order: &str, prefer_words: bool, sources: &[Arc<FakeSource>]) -> LyricsService {
    let settings = AppSettings {
        metadata: MetadataSettings {
            lyrics_sources: order.to_string(),
            prefer_word_timed_lyrics: prefer_words,
            ..MetadataSettings::default()
        },
        ..AppSettings::default()
    };
    LyricsService::new(
        sources
            .iter()
            .map(|s| s.clone() as Arc<dyn ILyricsSource>)
            .collect(),
        Arc::new(SettingsStore::for_tests(settings)),
    )
}

fn query() -> LyricsQuery {
    LyricsQuery::new("Artist", "Song", None, Some(200))
}

fn found(source: &str, synced: Option<&str>, plain: Option<&str>) -> LyricsLookup {
    LyricsLookup::new(
        Some(LyricsResult::new(
            source,
            synced.map(String::from),
            plain.map(String::from),
            false,
        )),
        false,
    )
}

fn none() -> CancellationToken {
    CancellationToken::new()
}

fn source_of(lookup: &LyricsLookup) -> &str {
    lookup.result.as_ref().map_or("", |result| result.source.as_str())
}

const WORDS: &str = "[00:01.00]<00:01.00>word <00:01.50>by word<00:02.00>";
const LINES: &str = "[00:01.00]line by line";

// ---- LyricsTests ----------------------------------------------------------------------------

// LyricsTests.Service_SyncedFromALaterSource_BeatsAnEarlierPlainOne
#[tokio::test]
async fn service_synced_from_a_later_source_beats_an_earlier_plain_one() {
    let plain = FakeSource::new("lrclib", || found("first", None, Some("plain words")));
    let synced = FakeSource::new("netease", || found("second", Some("[00:01.00]timed"), None));

    let lookup = service("lrclib,netease", true, &[plain, synced])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "second");
}

// LyricsTests.Service_NothingSynced_KeepsThePlainOne
#[tokio::test]
async fn service_nothing_synced_keeps_the_plain_one() {
    let plain = FakeSource::new("lyricsovh", || found("ovh", None, Some("words")));
    let none_found = FakeSource::new("lrclib", LyricsLookup::miss);

    let lookup = service("lrclib,lyricsovh", true, &[none_found, plain])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "ovh");
}

// LyricsTests.Service_SourceNotListed_IsNeverAsked
#[tokio::test]
async fn service_source_not_listed_is_never_asked() {
    let netease = FakeSource::new("netease", LyricsLookup::miss);
    let lrclib = FakeSource::new("lrclib", LyricsLookup::miss);

    service("lrclib,lyricsovh", true, &[lrclib.clone(), netease.clone()])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(netease.calls(), 0);
    assert_eq!(lrclib.calls(), 1);
}

// LyricsTests.Service_BusySource_IsNotRememberedAsNoLyrics
#[tokio::test]
async fn service_busy_source_is_not_remembered_as_no_lyrics() {
    let busy = Arc::new(AtomicBool::new(true));
    let flag = busy.clone();
    let source = FakeSource::new("lrclib", move || {
        if flag.load(Ordering::SeqCst) {
            LyricsLookup::failed()
        } else {
            found("lrclib", Some("[00:01.00]x"), None)
        }
    });
    let service = service("lrclib", true, &[source]);

    assert!(
        service
            .find(&query(), &none(), LyricsTiming::None)
            .await
            .transient
    );
    busy.store(false, Ordering::SeqCst);
    let lookup = service.find(&query(), &none(), LyricsTiming::None).await;
    assert!(lookup.result.expect("found").has_synced());
}

#[tokio::test]
async fn service_remembers_an_answer_and_a_miss_until_cleared() {
    let source = FakeSource::new("lrclib", LyricsLookup::miss);
    let service = service("lrclib", true, std::slice::from_ref(&source));

    service.find(&query(), &none(), LyricsTiming::None).await;
    service.find(&query(), &none(), LyricsTiming::None).await;
    assert_eq!(source.calls(), 1);
    // Another length is another song as far as the cache knows.
    service
        .find(
            &LyricsQuery::new("Artist", "Song", None, Some(201)),
            &none(),
            LyricsTiming::None,
        )
        .await;
    assert_eq!(source.calls(), 2);

    service.clear();
    service.find(&query(), &none(), LyricsTiming::None).await;
    assert_eq!(source.calls(), 3);

    // No artist or no title is a miss without asking.
    service
        .find(
            &LyricsQuery::new(" ", "Song", None, None),
            &none(),
            LyricsTiming::None,
        )
        .await;
    assert_eq!(source.calls(), 3);
}

#[tokio::test]
async fn enabled_lists_the_sources_that_are_on_in_their_order() {
    let kugou = FakeSource::new("kugou", LyricsLookup::miss);
    let lrclib = FakeSource::new("lrclib", LyricsLookup::miss);
    let service = service("lrclib,kugou", true, &[kugou, lrclib]);

    let keys: Vec<String> = service.enabled().iter().map(|s| s.key().to_string()).collect();
    assert_eq!(keys, ["lrclib", "kugou"]);
    assert!(service.source("netease").is_none());
}

// ---- LyricsSongSourceTests ------------------------------------------------------------------

// LyricsSongSourceTests.Ranked_LineTimedOwnLyricsFirst_LoseToWordsWhenWordsArePreferred
#[tokio::test]
async fn ranked_line_timed_own_lyrics_first_lose_to_words_when_words_are_preferred() {
    let kugou = FakeSource::new("kugou", || found("KuGou", Some(WORDS), None));

    let lookup = service("song,kugou", true, &[kugou])
        .find(&query(), &none(), LyricsTiming::Line)
        .await;

    assert_eq!(source_of(&lookup), "KuGou");
}

// LyricsSongSourceTests.Ranked_LineTimedOwnLyricsFirst_StandWhenWordsAreNotPreferred
#[tokio::test]
async fn ranked_line_timed_own_lyrics_first_stand_when_words_are_not_preferred() {
    let kugou = FakeSource::new("kugou", || found("KuGou", Some(WORDS), None));

    let lookup = service("song,kugou", false, std::slice::from_ref(&kugou))
        .find(&query(), &none(), LyricsTiming::Line)
        .await;

    assert!(lookup.result.expect("found").is_songs_own());
    assert_eq!(kugou.calls(), 0);
}

// LyricsSongSourceTests.Ranked_WordTimedOwnLyrics_EndTheSearch
#[tokio::test]
async fn ranked_word_timed_own_lyrics_end_the_search() {
    let kugou = FakeSource::new("kugou", || found("KuGou", Some(WORDS), None));

    let lookup = service("song,kugou", true, std::slice::from_ref(&kugou))
        .find(&query(), &none(), LyricsTiming::Word)
        .await;

    assert!(lookup.result.expect("found").is_songs_own());
    assert_eq!(kugou.calls(), 0);
}

// LyricsSongSourceTests.Ranked_ASourceAboveTheSong_WinsAtTheSameTiming
#[tokio::test]
async fn ranked_a_source_above_the_song_wins_at_the_same_timing() {
    let lrclib = FakeSource::new("lrclib", || found("LRCLIB", Some(LINES), None));

    let lookup = service("lrclib,song", false, &[lrclib])
        .find(&query(), &none(), LyricsTiming::Line)
        .await;

    assert_eq!(source_of(&lookup), "LRCLIB");
}

// LyricsSongSourceTests.Ranked_PlainOwnLyrics_LoseToTimedOnesBelowThem
#[tokio::test]
async fn ranked_plain_own_lyrics_lose_to_timed_ones_below_them() {
    let lrclib = FakeSource::new("lrclib", || found("LRCLIB", Some(LINES), None));

    let lookup = service("song,lrclib", false, &[lrclib])
        .find(&query(), &none(), LyricsTiming::Plain)
        .await;

    assert_eq!(source_of(&lookup), "LRCLIB");
}

// LyricsSongSourceTests.Ranked_NothingBetterFound_TheSongsOwnStand
#[tokio::test]
async fn ranked_nothing_better_found_the_songs_own_stand() {
    let lrclib = FakeSource::new("lrclib", LyricsLookup::miss);

    let lookup = service("song,lrclib", true, &[lrclib])
        .find(&query(), &none(), LyricsTiming::Line)
        .await;

    let result = lookup.result.expect("found");
    assert!(result.is_songs_own());
    assert_eq!(result.timing(), LyricsTiming::Line);
}

// ---- LyricsWordTimingTests ------------------------------------------------------------------

// LyricsWordTimingTests.Order_PreferWordTimed_ALaterWordTimedAnswerBeatsAnEarlierLineTimedOne
#[tokio::test]
async fn order_prefer_word_timed_a_later_word_timed_answer_beats_an_earlier_line_timed_one() {
    let lines = FakeSource::new("lrclib", || found("LRCLIB", Some("[00:01.00]line"), None));
    let words = FakeSource::new("kugou", || found("KuGou", Some("[00:01.00]<00:01.00>word"), None));

    let lookup = service("lrclib,kugou", true, &[lines, words])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "KuGou");
}

// LyricsWordTimingTests.Order_PreferWordTimedOff_TheFirstTimedAnswerWins
#[tokio::test]
async fn order_prefer_word_timed_off_the_first_timed_answer_wins() {
    let lines = FakeSource::new("lrclib", || found("LRCLIB", Some("[00:01.00]line"), None));
    let words = FakeSource::new("kugou", || found("KuGou", Some("[00:01.00]<00:01.00>word"), None));

    let lookup = service("lrclib,kugou", false, &[lines, words.clone()])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "LRCLIB");
    assert_eq!(words.calls(), 0);
}

// LyricsWordTimingTests.Order_WordTimedFirst_NothingLaterIsAsked
#[tokio::test]
async fn order_word_timed_first_nothing_later_is_asked() {
    let words = FakeSource::new("kugou", || found("KuGou", Some("[00:01.00]<00:01.00>word"), None));
    let lines = FakeSource::new("lrclib", || found("LRCLIB", Some("[00:01.00]line"), None));

    let lookup = service("kugou,lrclib", true, &[words, lines.clone()])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "KuGou");
    assert_eq!(lines.calls(), 0);
}

// LyricsWordTimingTests.Order_NoWordTimingAnywhere_KeepsTheLineTimedOne
#[tokio::test]
async fn order_no_word_timing_anywhere_keeps_the_line_timed_one() {
    let lines = FakeSource::new("kugou", || found("KuGou", Some("[00:01.00]line"), None));
    let plain = FakeSource::new("lrclib", || found("LRCLIB", None, Some("words")));

    let lookup = service("kugou,lrclib", true, &[lines, plain])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "KuGou");
}

// LyricsWordTimingTests.Order_KugouLeftOut_IsNeverAsked
#[tokio::test]
async fn order_kugou_left_out_is_never_asked() {
    let kugou = FakeSource::new("kugou", || found("KuGou", Some("[00:01.00]<00:01.00>word"), None));
    let lrclib = FakeSource::new("lrclib", LyricsLookup::miss);

    service("lrclib", true, &[kugou.clone(), lrclib])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert_eq!(kugou.calls(), 0);
}

// LyricsWordTimingTests.Order_OutOfTime_ReturnsTheBestFoundSoFar
#[tokio::test]
async fn order_out_of_time_returns_the_best_found_so_far() {
    let budget = CancellationToken::new();
    let lines = FakeSource::new("lrclib", || found("LRCLIB", Some("[00:01.00]line"), None));
    let cancel = budget.clone();
    let slow = FakeSource::new("kugou", move || {
        cancel.cancel();
        LyricsLookup::failed()
    });

    let lookup = service("lrclib,kugou", true, &[lines, slow])
        .find(&query(), &budget, LyricsTiming::None)
        .await;

    assert_eq!(source_of(&lookup), "LRCLIB");
}

#[tokio::test]
async fn an_instrumental_answer_ends_the_search() {
    let instrumental = FakeSource::new("lrclib", || {
        LyricsLookup::new(Some(LyricsResult::new("LRCLIB", None, None, true)), false)
    });
    let words = FakeSource::new("kugou", || found("KuGou", Some(WORDS), None));

    let lookup = service("lrclib,kugou", true, &[instrumental, words.clone()])
        .find(&query(), &none(), LyricsTiming::None)
        .await;

    assert!(lookup.result.expect("an answer").instrumental);
    assert_eq!(words.calls(), 0);
}
