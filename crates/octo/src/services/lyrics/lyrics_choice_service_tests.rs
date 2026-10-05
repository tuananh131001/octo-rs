//! `LyricsChoiceService`, through the service rather than the Subsonic endpoints (those are
//! 6-A's): the intent of LyricsChoiceTests' Candidates_*, Pin_* and Hide_* tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use octo_core::lyrics::{
    ILyricsSource, LyricsCandidate, LyricsLookup, LyricsPin, LyricsQuery, LyricsResult, LyricsSearch,
};
use octo_core::settings::{AppSettings, MetadataSettings, SettingsStore};
use tokio_util::sync::CancellationToken;

use super::{LyricsChoiceService, LyricsChoiceStore};
use crate::services::lyrics::lyrics_service::LyricsService;

/// LyricsChoiceTests.ChoosableSource: a source whose search and fetch the tests control.
struct ChoosableSource {
    key: String,
    entries: Vec<LyricsCandidate>,
    lyrics: HashMap<String, LyricsResult>,
    fetches: AtomicUsize,
}

#[async_trait]
impl ILyricsSource for ChoosableSource {
    fn key(&self) -> &str {
        &self.key
    }

    async fn find(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsLookup {
        LyricsLookup::new(
            Some(LyricsResult::new(
                "KuGou",
                Some("[00:01.00]automatic words".into()),
                None,
                false,
            )),
            false,
        )
    }

    async fn search(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsSearch {
        LyricsSearch::new(self.entries.clone(), false)
    }

    async fn fetch(&self, id: &str, _ct: &CancellationToken) -> LyricsLookup {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        match self.lyrics.get(id) {
            Some(found) => LyricsLookup::new(Some(found.clone()), false),
            None => LyricsLookup::miss(),
        }
    }
}

/// LyricsChoiceTests.Kugou(): the song and its remix, listed remix first.
fn kugou() -> Arc<ChoosableSource> {
    let lyrics = HashMap::from([
        (
            "1.a".to_string(),
            LyricsResult::new(
                "KuGou",
                Some(
                    "[00:01.00]<00:01.00>right <00:01.50>words\n[00:03.00]<00:03.00>second<00:04.00>".into(),
                ),
                None,
                false,
            ),
        ),
        (
            "2.b".to_string(),
            LyricsResult::new("KuGou", Some("[00:01.00]remix words".into()), None, false),
        ),
    ]);
    Arc::new(ChoosableSource {
        key: "kugou".into(),
        entries: vec![
            LyricsCandidate::new(
                "kugou",
                "2.b",
                "Some Song (Remix)",
                "Some Artist",
                None,
                Some(260),
            ),
            LyricsCandidate::new(
                "kugou",
                "1.a",
                "Some Song",
                "Some Artist",
                Some("The Album".into()),
                Some(200),
            ),
        ],
        lyrics,
        fetches: AtomicUsize::new(0),
    })
}

fn service(source: Arc<ChoosableSource>) -> (LyricsChoiceService, Arc<LyricsChoiceStore>) {
    let settings = AppSettings {
        metadata: MetadataSettings {
            lyrics_sources: "kugou".into(),
            ..MetadataSettings::default()
        },
        ..AppSettings::default()
    };
    let lyrics = Arc::new(LyricsService::new(
        vec![source as Arc<dyn ILyricsSource>],
        Arc::new(SettingsStore::for_tests(settings)),
    ));
    let store = Arc::new(LyricsChoiceStore::new(None));
    (LyricsChoiceService::new(lyrics, store.clone()), store)
}

fn query() -> LyricsQuery {
    LyricsQuery::new("Some Artist", "Some Song", None, Some(200))
}

fn none() -> CancellationToken {
    CancellationToken::new()
}

// LyricsChoiceTests.Candidates_ListEveryEntryTheSameSongFirstWithAPreview (the service's half)
#[tokio::test]
async fn candidates_list_every_entry_the_same_song_first_with_a_preview() {
    let (choices, _) = service(kugou());

    let candidates = choices.candidates(&query(), &none()).await;

    let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["kugou:1.a", "kugou:2.b"]);
    let first = &candidates[0];
    assert_eq!(first.source, "kugou");
    assert_eq!(first.kind, "word");
    assert_eq!(first.album.as_deref(), Some("The Album"));
    assert_eq!(first.duration_seconds, Some(200));
    assert!(first.same_song);
    assert_eq!(first.preview, ["right words", "second"]);
    assert!(!candidates[1].same_song);
    assert_eq!(candidates[1].kind, "line");
}

// LyricsChoiceTests.Candidates_ManualSearch_JudgesByTheTitleAndArtistGiven (the service's half)
#[tokio::test]
async fn candidates_are_judged_by_the_title_and_artist_asked() {
    let (choices, _) = service(kugou());

    let candidates = choices
        .candidates(&LyricsQuery::new("Wrong Tag", "Track 01", None, None), &none())
        .await;
    assert!(candidates.iter().all(|c| !c.same_song));

    let candidates = choices
        .candidates(&LyricsQuery::new("Some Artist", "Some Song", None, None), &none())
        .await;
    assert!(candidates[0].same_song);
}

// LyricsChoiceTests.Pin_EveryClientGetsThePinnedLyrics_AndAutoGoesBack (the service's half)
#[tokio::test]
async fn pin_from_the_list_just_shown_then_auto_goes_back() {
    let source = kugou();
    let (choices, _) = service(source.clone());
    choices.candidates(&query(), &none()).await;
    let fetched = source.fetches.load(Ordering::SeqCst);

    assert!(
        choices
            .pin(
                "ext-1",
                "kugou:1.a",
                Some("Some Artist"),
                Some("Some Song"),
                Some("alice"),
                &none()
            )
            .await
    );
    // Remembered from the list: no second fetch.
    assert_eq!(source.fetches.load(Ordering::SeqCst), fetched);

    let pin = choices.pin_for("ext-1").expect("pinned");
    assert_eq!(pin.choice, "kugou:1.a");
    assert_eq!(pin.source.as_deref(), Some("KuGou"));
    assert_eq!(pin.set_by.as_deref(), Some("alice"));
    assert_eq!(choices.choice_for("ext-1"), "kugou:1.a");
    assert!(choices.any_pins());

    assert!(choices.clear("ext-1"));
    assert_eq!(choices.choice_for("ext-1"), LyricsPin::AUTO);
    assert!(!choices.clear("ext-1"));
}

// LyricsChoiceTests.Pin_WithoutAListFirst_FetchesTheCandidateFromItsSource
#[tokio::test]
async fn pin_without_a_list_first_fetches_the_candidate_from_its_source() {
    let (choices, _) = service(kugou());

    assert!(choices.pin("ext-1", "kugou:1.a", None, None, None, &none()).await);
    assert!(!choices.pin("ext-1", "kugou:9.z", None, None, None, &none()).await);
    assert!(!choices.pin("ext-1", "nosuch:1", None, None, None, &none()).await);
    assert!(!choices.pin("ext-1", ":1.a", None, None, None, &none()).await);

    assert_eq!(choices.choice_for("ext-1"), "kugou:1.a");
    let lyrics = choices.lyrics_of("kugou:1.a", &none()).await.expect("found");
    assert_eq!(lyrics.candidate_id.as_deref(), Some("kugou:1.a"));
}

// LyricsChoiceTests.Hide_EveryClientGetsNoLyrics_EvenNavidromesOwn (the service's half)
#[tokio::test]
async fn hide_pins_none() {
    let (choices, _) = service(kugou());

    choices.hide("lib1", Some("Some Artist"), Some("Library Song"), None);

    let pin = choices.pin_for("lib1").expect("pinned");
    assert!(pin.is_hidden());
    assert!(pin.lyrics().is_none());
    assert_eq!(choices.choice_for("lib1"), LyricsPin::HIDDEN);
    assert_eq!(
        choices
            .pin_for_name("Some Artist", "Library Song")
            .expect("by name")
            .song_id,
        "lib1"
    );
}

/// A pin made when the song had another id (its file was replaced by a better copy) still
/// answers for it, found by artist and title, and Automatic clears it for good.
// LyricsChoiceTests.Pin_FollowsTheSongToItsNewId_AndAutomaticClearsIt (the service's half)
#[tokio::test]
async fn pin_follows_the_song_to_its_new_id_and_automatic_clears_it() {
    let (choices, store) = service(kugou());
    store.set(LyricsPin::new(
        "old-id",
        "kugou:1.a",
        Some("KuGou".into()),
        Some("[00:01.00]<00:01.00>pinned <00:01.50>words".into()),
        None,
        Some("Some Artist".into()),
        Some("Library Song".into()),
        Some("alice".into()),
        Utc::now(),
    ));

    assert_eq!(
        choices.choice_for_song("lib1", Some("Some Artist"), Some("Library Song")),
        "kugou:1.a"
    );
    assert!(
        choices
            .pin_for_song("lib1", Some(" "), Some("Library Song"))
            .is_none()
    );
    assert_eq!(choices.choice_for("lib1"), LyricsPin::AUTO);

    assert!(choices.clear_song("lib1", Some("Some Artist"), Some("Library Song")));
    assert!(store.all().is_empty());
}

#[tokio::test]
async fn a_cancelled_request_offers_nothing_it_had_not_fetched() {
    let (choices, _) = service(kugou());
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    assert!(choices.candidates(&query(), &cancelled).await.is_empty());
}

#[test]
fn kind_of_names_the_timing() {
    let plain = LyricsResult::new("x", None, Some("words".into()), false);
    assert_eq!(LyricsChoiceService::kind_of(&plain), "plain");
    let instrumental = LyricsResult::new("x", None, None, true);
    assert_eq!(LyricsChoiceService::kind_of(&instrumental), "instrumental");
}
