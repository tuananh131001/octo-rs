//! The KuGou parts of LyricsWordTimingTests, SongIdentityTests and QueryVariantLookupTests.

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use octo_core::common::{SongIdentity, SongVerdict};
use octo_core::lyrics::{ILyricsSource, LyricsCandidate, LyricsQuery, LyricsText, LyricsTiming};
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

use super::{KRC_KEY, KugouLyricsSource};
use crate::services::lyrics::lyrics_http::kugou_http_client;
use crate::services::lyrics::test_support::{FakeHttp, json, status};

/// SongIdentityTests.OtherVersions: never the same song's lyrics.
pub(crate) const OTHER_VERSIONS: [(&str, &str); 7] = [
    ("Creep", "Creep (Live)"),
    ("Heat Waves", "Heat Waves (Sped Up)"),
    ("Heat Waves", "Heat Waves Slowed"),
    ("Mask Off", "Mask Off Remix"),
    ("Song", "Song (Instrumental)"),
    ("Song (Skrillex Remix)", "Song (Diplo Remix)"),
    ("Song", "Song - Radio Edit"),
];

/// A KRC file made the way KuGou makes them: UTF-8 with a byte order mark, zlib, XORed with
/// the key, behind "krc1".
pub(crate) fn encode_krc(text: &str) -> Vec<u8> {
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::best());
    zlib.write_all(format!("\u{FEFF}{text}").as_bytes())
        .expect("compressed");
    let mut body = zlib.finish().expect("compressed");
    for (index, byte) in body.iter_mut().enumerate() {
        *byte ^= KRC_KEY[index % KRC_KEY.len()];
    }
    [b"krc1".as_slice(), &body].concat()
}

const KRC: &str = "[ti:stronger]
[ar:kanye west]
[offset:0]
[language:eyJjb250ZW50IjpbXX0=]
[456,1724]<0,486,0>Work <486,215,0>it  <701,261,0>make <962,178,0>it
[1880,3397]<0,584,0>Makes <584,402,0>us <986,352,0>harder";

fn none() -> CancellationToken {
    CancellationToken::new()
}

// LyricsWordTimingTests.Krc_DecodesBackToItsText
#[test]
fn krc_decodes_back_to_its_text() {
    assert_eq!(
        KugouLyricsSource::decode_krc(&encode_krc(KRC)).as_deref(),
        Some(KRC)
    );
}

// LyricsWordTimingTests.Krc_NotAKrcFile_IsNull
#[test]
fn krc_not_a_krc_file_is_null() {
    assert_eq!(KugouLyricsSource::decode_krc(b"[00:01.00]plain lrc"), None);
    assert_eq!(KugouLyricsSource::decode_krc(b"krc1\x01\x02\x03\x04"), None);
}

// LyricsWordTimingTests.Krc_ParsesLinesAndWordsWithTheirTimes
#[test]
fn krc_parses_lines_and_words_with_their_times() {
    let lines = KugouLyricsSource::parse_krc(KRC);

    assert_eq!(lines.len(), 2);
    let first = &lines[0];
    assert_eq!(first.start_ms, 456);
    // Two spaces after "it" become one, and the line ends without one.
    assert_eq!(first.text, "Work it make it");
    let starts: Vec<i64> = first.words.iter().map(|w| w.start_ms).collect();
    assert_eq!(starts, [456, 942, 1157, 1418]);
    let ends: Vec<i64> = first.words.iter().map(|w| w.end_ms.expect("an end")).collect();
    assert_eq!(ends, [942, 1157, 1418, 1596]);
    let texts: Vec<&str> = first.words.iter().map(|w| first.word_text(w)).collect();
    assert_eq!(texts, ["Work ", "it ", "make ", "it"]);
    assert_eq!(first.end_ms, Some(1596));
}

// LyricsWordTimingTests.Krc_BecomesEnhancedLrcThatReadsBackTheSame (its ParseKrc half)
#[test]
fn krc_becomes_enhanced_lrc_that_reads_back_the_same() {
    let lrc = LyricsText::write_lrc(&KugouLyricsSource::parse_krc(KRC));

    assert!(
        lrc.starts_with("[00:00.45]<00:00.45>Work <00:00.94>it <00:01.15>make <00:01.41>it<00:01.59>"),
        "{lrc}"
    );
    let back = LyricsText::parse_lrc(&lrc);
    assert_eq!(back[0].text, "Work it make it");
    let starts: Vec<i64> = back[0].words.iter().map(|w| w.start_ms).collect();
    assert_eq!(starts, [450, 940, 1150, 1410]);
    assert_eq!(back[0].end_ms, Some(1590));
}

#[test]
fn krc_a_line_without_word_tags_and_entities_and_a_number_too_big() {
    let lines = KugouLyricsSource::parse_krc(
        "[2000,500] I&apos;m here \r\n[99999999999999999999,1]<0,1,0>skipped\n[1000,0]<0,0,0>&amp;<5,0,0>  <9,0,0>x",
    );
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "& x");
    assert_eq!(lines[0].end_ms, None);
    assert_eq!(lines[0].words.len(), 2);
    assert_eq!(lines[0].word_text(&lines[0].words[1]), "x");
    assert_eq!(lines[1].text, "I'm here");
    assert_eq!(lines[1].end_ms, Some(2500));
}

// SongIdentityTests.Lyrics_KuGou_NeverTakesAnotherVersion (7 rows)
#[test]
fn lyrics_kugou_never_takes_another_version() {
    for (want, got) in OTHER_VERSIONS {
        let query = LyricsQuery::new("Artist", want, None, Some(200));
        assert!(
            !KugouLyricsSource::is_this_song(
                &LyricsCandidate::new("kugou", "1.a", got, "Artist", None, Some(200)),
                &query
            ),
            "{want} / {got}"
        );
        assert!(
            !KugouLyricsSource::is_this_song(
                &LyricsCandidate::new("kugou", "1.a", want, "Artist", None, Some(200)),
                &LyricsQuery::new("Artist", got, None, Some(200))
            ),
            "{got} / {want}"
        );
    }
}

// SongIdentityTests.Lyrics_TheSameSongWrittenOtherwise_IsFound (4 rows)
#[test]
fn lyrics_the_same_song_written_otherwise_is_found() {
    let cases = [
        ("$UICIDE", "$uicideboy$", "Suicide", "Suicideboys"),
        ("Huntin’ Wabbitz", "$uicideboy$", "Huntin' Wabbitz", "$UICIDEBOY$"),
        (
            "Can't Tell Me Nothing",
            "Kanye West",
            "Can't Tell Me Nothing (Explicit)",
            "Ye (侃爷)",
        ),
        (
            "Sunlight On Your Skin",
            "Lil Peep feat. iLoveMakonnen",
            "Sunlight On Your Skin",
            "Lil Peep、iLoveMakonnen",
        ),
    ];
    for (want, want_artist, got, got_artist) in cases {
        assert!(
            KugouLyricsSource::is_this_song(
                &LyricsCandidate::new("kugou", "1.a", got, got_artist, None, Some(200)),
                &LyricsQuery::new(want_artist, want, None, Some(201))
            ),
            "{want} / {got}"
        );
    }
}

// SongIdentityTests.Lyrics_ACleanEditsWordsFitTheSong (3 rows; its KuGou half, with the
// SongIdentity half again for the contrast)
#[test]
fn lyrics_a_clean_edits_words_fit_the_song() {
    let cases = [
        ("Movie Star", "Movie Star (Clean)"),
        ("Movie Star", "Movie Star (Clean Version)"),
        ("Movie Star (Explicit)", "Movie Star (Censored)"),
    ];
    for (want, got) in cases {
        // Same words at the same times, a few bleeped: good lyrics for the explicit recording.
        assert!(
            KugouLyricsSource::is_this_song(
                &LyricsCandidate::new("kugou", "1.a", got, "Artist", None, Some(200)),
                &LyricsQuery::new("Artist", want, None, Some(200))
            ),
            "{want} / {got}"
        );
        // A download still never takes the clean edit for the song asked for.
        assert_ne!(
            SongIdentity::same_text(want, "Artist", got, "Artist", None).verdict,
            SongVerdict::Same,
            "{want} / {got}"
        );
    }
}

/// LyricsWordTimingTests.FakeKugou: the lyric search, the catalogue, the search by hash and the
/// downloads, each answered from what the test set, on one local server.
#[derive(Clone, Default)]
struct FakeKugou {
    search_body: Option<String>,
    catalogue_body: Option<String>,
    hash_search_body: Option<String>,
    krc: Option<Vec<u8>>,
    lrc: Option<String>,
    fail_with: Option<u16>,
}

impl FakeKugou {
    fn answer(&self, url: &str) -> ResponseTemplate {
        if let Some(code) = self.fail_with {
            return ResponseTemplate::new(code);
        }
        let empty = r#"{"status":200,"candidates":[]}"#;
        let body = if url.contains("/api/v3/search/song") {
            self.catalogue_body
                .clone()
                .unwrap_or_else(|| r#"{"status":1,"data":{"info":[]}}"#.into())
        } else if url.contains("/search?") && url.contains("hash=") {
            self.hash_search_body.clone().unwrap_or_else(|| empty.into())
        } else if url.contains("/search?") {
            self.search_body.clone().unwrap_or_else(|| empty.into())
        } else if url.contains("fmt=krc") {
            match &self.krc {
                None => r#"{"status":404,"content":""}"#.into(),
                Some(krc) => format!(
                    r#"{{"status":200,"content":"{}","fmt":"krc","contenttype":0}}"#,
                    STANDARD.encode(krc)
                ),
            }
        } else if url.contains("fmt=lrc") {
            match &self.lrc {
                None => r#"{"status":404,"content":""}"#.into(),
                Some(lrc) => format!(
                    r#"{{"status":200,"content":"{}","fmt":"lrc"}}"#,
                    STANDARD.encode(lrc)
                ),
            }
        } else {
            "{}".into()
        };
        json(&body)
    }

    async fn start(self) -> (FakeHttp, KugouLyricsSource) {
        let http = FakeHttp::start(move |url| self.answer(url)).await;
        let source = KugouLyricsSource::with_base_urls(kugou_http_client(), &http.uri(), &http.uri());
        (http, source)
    }
}

fn candidate(id: &str, song: &str, singer: &str, ms: i64) -> String {
    format!(
        r#"{{"id":"{id}","accesskey":"KEY{id}","song":"{song}","singer":"{singer}","duration":{ms},"krctype":2,"score":60}}"#
    )
}

// LyricsWordTimingTests.Kugou_OnlyAnotherSongOfTheSameLength_IsAMissAndNothingIsDownloaded
#[tokio::test]
async fn kugou_only_another_song_of_the_same_length_is_a_miss_and_nothing_is_downloaded() {
    let (http, source) = FakeKugou {
        search_body: Some(format!(
            r#"{{"status":200,"candidates":[{},{}]}}"#,
            candidate("1", "Ultimate $uicide", "$uicideboy$", 170_000),
            candidate("2", "Black $uicide", "$uicideboy$", 170_500)
        )),
        catalogue_body: Some(
            r#"{"status":1,"data":{"info":[{"hash":"h1","songname":"Ultimate $uicide (Explicit)","singername":"$uicideboy$","duration":170}]}}"#.into(),
        ),
        krc: Some(encode_krc(KRC)),
        ..FakeKugou::default()
    }
    .start()
    .await;

    let lookup = source
        .find(
            &LyricsQuery::new("$uicideboy$", "$UICIDE", None, Some(169)),
            &none(),
        )
        .await;

    assert!(lookup.result.is_none());
    assert!(!lookup.transient);
    assert!(!http.calls().await.iter().any(|call| call.contains("/download")));
}

// LyricsWordTimingTests.Kugou_TheSong_GivesWordTimedLyricsWithoutItsCreditsOrTitleLine
#[tokio::test]
async fn kugou_the_song_gives_word_timed_lyrics_without_its_credits_or_title_line() {
    let krc = "[0,1000]<0,500,0>Kanye West <500,500,0>- Stronger
[1000,900]<0,900,0>Producer：Daft Punk
[456000,1724]<0,486,0>Work <486,215,0>it";
    let (_http, source) = FakeKugou {
        search_body: Some(format!(
            r#"{{"status":200,"candidates":[{}]}}"#,
            candidate("9", "Stronger", "Kanye West", 312_006)
        )),
        krc: Some(encode_krc(krc)),
        ..FakeKugou::default()
    }
    .start()
    .await;

    let lookup = source
        .find(
            &LyricsQuery::new("Kanye West", "Stronger", Some("Graduation".into()), Some(311)),
            &none(),
        )
        .await;

    let result = lookup.result.expect("found");
    assert_eq!(result.timing(), LyricsTiming::Word);
    assert_eq!(result.candidate_id.as_deref(), Some("kugou:9.KEY9"));
    assert_eq!(result.doubt, None);
    let lines = LyricsText::parse_lrc(result.synced.as_deref().unwrap_or(""));
    let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(texts, ["Work it"]);
}

/// What the 300-song evaluation found KuGou putting in its lyrics: who sings next, HTML
/// entities, a credit timed before its line, and a title line naming the song.
// LyricsWordTimingTests.Kugou_Clean_TakesOutSpeakersEntitiesCreditsAndTheTitleLine
#[test]
fn kugou_clean_takes_out_speakers_entities_credits_and_the_title_line() {
    let krc = "[0,500]<0,500,0>Sunlight On Your Skin (Explicit) - Lil Peep/iLoveMakonnen
[100,300]<-100,100,0>Written by：Drake
[500,500]<0,500,0>Lil Peep：
[1000,2000]<0,400,0>Kanye West：<400,600,0>Real <1000,500,0>friends
[4000,1000]<0,500,0>I&apos;m <500,500,0>cruising";
    let lines = KugouLyricsSource::clean(
        &KugouLyricsSource::parse_krc(krc),
        Some(&LyricsQuery::new(
            "Lil Peep",
            "Sunlight On Your Skin",
            None,
            Some(200),
        )),
    );
    let lrc = LyricsText::strip_credits(&LyricsText::write_lrc(&lines));
    let back = LyricsText::parse_lrc(&lrc);

    let texts: Vec<&str> = back.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(texts, ["Real friends", "I'm cruising"]);
    let words: Vec<&str> = back[0].words.iter().map(|w| back[0].word_text(w)).collect();
    assert_eq!(words, ["Real ", "friends"]);
    assert_eq!(back[0].words[0].start_ms, 1400);
}

// LyricsWordTimingTests.Kugou_Clean_FindsTheNameLineAfterTheCredits (2 rows)
#[test]
fn kugou_clean_finds_the_name_line_after_the_credits() {
    let cases = [
        (
            "Can't Tell Me Nothing - Ye",
            "Kanye West",
            "Can't Tell Me Nothing",
        ),
        (
            "Best Friend (Explicit) - Yelawolf (亚拉狼)/Eminem",
            "Yelawolf • Eminem",
            "Best Friend",
        ),
    ];
    for (name_line, artist, title) in cases {
        let lines = KugouLyricsSource::parse_krc(&format!(
            "[0,10]<-100,100,0>Lyrics by：Someone\n[10,10]<0,10,0>{name_line}\n[5000,500]<0,500,0>sung"
        ));

        let kept = KugouLyricsSource::clean(&lines, Some(&LyricsQuery::new(artist, title, None, Some(200))));

        let back = LyricsText::parse_lrc(&LyricsText::strip_credits(&LyricsText::write_lrc(&kept)));
        let texts: Vec<&str> = back.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts, ["sung"], "{name_line}");
    }
}

#[test]
fn kugou_clean_counts_a_speakers_name_in_utf16_units() {
    // 60 units is a speaker; 61 (an emoji is two) is a lyric.
    let sixty = format!("{}：words", "a".repeat(60));
    let sixty_one = format!("{}😀：words", "a".repeat(59));
    let lines = vec![
        octo_core::lyrics::LyricLine::new(1000, sixty),
        octo_core::lyrics::LyricLine::new(2000, sixty_one.clone()),
    ];
    let kept = KugouLyricsSource::clean(&lines, None);
    assert_eq!(kept[0].text, "words");
    assert_eq!(kept[1].text, sixty_one);
}

// LyricsWordTimingTests.Kugou_NoKrc_TakesItsLrc
#[tokio::test]
async fn kugou_no_krc_takes_its_lrc() {
    let (_http, source) = FakeKugou {
        search_body: Some(format!(
            r#"{{"status":200,"candidates":[{}]}}"#,
            candidate("5", "Song", "Someone", 200_000)
        )),
        lrc: Some("[00:12.00]a line\n[00:15.00]another".into()),
        ..FakeKugou::default()
    }
    .start()
    .await;

    let lookup = source
        .find(&LyricsQuery::new("Someone", "Song", None, Some(200)), &none())
        .await;

    assert_eq!(lookup.result.expect("found").timing(), LyricsTiming::Line);
}

#[tokio::test]
async fn kugou_plain_text_and_the_instrumental_mark() {
    let (_http, source) = FakeKugou {
        lrc: Some("纯音乐，请欣赏".into()),
        ..FakeKugou::default()
    }
    .start()
    .await;
    let instrumental = source.fetch("5.K", &none()).await.result.expect("an answer");
    assert!(instrumental.instrumental);

    let (_http, source) = FakeKugou {
        lrc: Some("Just words\r\nno times ".into()),
        ..FakeKugou::default()
    }
    .start()
    .await;
    let plain = source.fetch("5.K", &none()).await.result.expect("an answer");
    assert_eq!(plain.plain.as_deref(), Some("Just words\nno times"));

    assert_eq!(
        source.fetch(".K", &none()).await,
        octo_core::lyrics::LyricsLookup::miss()
    );
    assert_eq!(
        source.fetch("5.", &none()).await,
        octo_core::lyrics::LyricsLookup::miss()
    );
}

// LyricsWordTimingTests.Kugou_NoLyricEntry_FindsTheSongInTheCatalogueAndAsksByItsHash
#[tokio::test]
async fn kugou_no_lyric_entry_finds_the_song_in_the_catalogue_and_asks_by_its_hash() {
    let (http, source) = FakeKugou {
        catalogue_body: Some(
            r#"{"status":1,"data":{"info":[{"hash":"abc","songname":"Headlines (Explicit)","singername":"Drake","album_name":"Take Care","duration":236}]}}"#.into(),
        ),
        hash_search_body: Some(format!(
            r#"{{"status":200,"candidates":[{}]}}"#,
            candidate("3", "Headlines", "Drake", 235_000)
        )),
        krc: Some(encode_krc(KRC)),
        ..FakeKugou::default()
    }
    .start()
    .await;

    let lookup = source
        .find(&LyricsQuery::new("Drake", "Headlines", None, Some(235)), &none())
        .await;

    assert_eq!(lookup.result.expect("found").timing(), LyricsTiming::Word);
    assert!(http.calls().await.iter().any(|call| call.contains("hash=abc")));
}

// LyricsWordTimingTests.Kugou_FailingOver_StopsAskingForAWhile
#[tokio::test]
async fn kugou_failing_over_stops_asking_for_a_while() {
    let (http, source) = FakeKugou {
        fail_with: Some(500),
        ..FakeKugou::default()
    }
    .start()
    .await;
    let query = LyricsQuery::new("A", "B", None, Some(100));

    for _ in 0..5 {
        assert!(source.find(&query, &none()).await.transient);
    }
    let before = http.calls().await.len();
    let after = source.find(&query, &none()).await;

    assert!(after.transient);
    assert_eq!(http.calls().await.len(), before);
}

// LyricsWordTimingTests.Kugou_RateLimited_CoolsDownWithoutAnotherRequest
#[tokio::test]
async fn kugou_rate_limited_cools_down_without_another_request() {
    let (http, source) = FakeKugou {
        fail_with: Some(429),
        ..FakeKugou::default()
    }
    .start()
    .await;

    assert!(
        source
            .find(&LyricsQuery::new("A", "B", None, Some(100)), &none())
            .await
            .transient
    );
    assert!(
        source
            .find(&LyricsQuery::new("C", "D", None, Some(100)), &none())
            .await
            .transient
    );
    assert_eq!(http.calls().await.len(), 1);
    assert!(source.cool_down_until().is_some());
}

/// QueryVariantLookupTests.Http: answers by the first route whose needle the unescaped URL
/// contains (or ends with, for a needle ending in $); anything else is a 404.
pub(crate) async fn routes(routes: &[(&str, String)]) -> FakeHttp {
    let routes: Vec<(String, String)> = routes.iter().map(|(n, b)| (n.to_string(), b.clone())).collect();
    FakeHttp::start(move |url| {
        for (needle, body) in &routes {
            let hit = match needle.strip_suffix('$') {
                Some(end) => url.ends_with(end),
                None => url.contains(needle.as_str()),
            };
            if hit {
                return json(body);
            }
        }
        status(404, "{}")
    })
    .await
}

fn base64(text: &str) -> String {
    STANDARD.encode(text)
}

// QueryVariantLookupTests.KuGou_FindsTheSongUnderItsSpelledOutName
#[tokio::test]
async fn kugou_finds_the_song_under_its_spelled_out_name() {
    let http = routes(&[
        (
            "keyword=suicideboys - SUICIDE",
            r#"{"status":200,"candidates":[{"id":"5","accesskey":"K5","song":"Suicide","singer":"Suicideboys","duration":170000}]}"#.into(),
        ),
        ("keyword=", r#"{"status":200,"candidates":[]}"#.into()),
        ("fmt=krc", r#"{"status":404,"content":""}"#.into()),
        ("fmt=lrc", format!(r#"{{"status":200,"content":"{}"}}"#, base64("[00:01.00]Line one"))),
    ])
    .await;
    let source = KugouLyricsSource::with_base_urls(kugou_http_client(), &http.uri(), &http.uri());

    let lookup = source
        .find(
            &LyricsQuery::new("$uicideboy$", "$UICIDE", None, Some(170)),
            &none(),
        )
        .await;

    assert_eq!(
        lookup.result.and_then(|r| r.candidate_id).as_deref(),
        Some("kugou:5.K5")
    );
    let calls = http.calls().await;
    assert!(
        calls.iter().any(|c| c.contains("keyword=$uicideboy$ - $UICIDE")),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("/api/v3/search/song")),
        "{calls:?}"
    );
}

// QueryVariantLookupTests.KuGou_AVariantThatFindsOnlyAnotherVersion_IsStillAMiss
#[tokio::test]
async fn kugou_a_variant_that_finds_only_another_version_is_still_a_miss() {
    let http = routes(&[
        (
            "keyword=suicideboys - SUICIDE",
            r#"{"status":200,"candidates":[{"id":"5","accesskey":"K5","song":"Suicide (Live)","singer":"Suicideboys","duration":170000}]}"#.into(),
        ),
        ("keyword=", r#"{"status":200,"candidates":[]}"#.into()),
        ("/api/v3/search/song", r#"{"status":1,"data":{"info":[]}}"#.into()),
    ])
    .await;
    let source = KugouLyricsSource::with_base_urls(kugou_http_client(), &http.uri(), &http.uri());

    let lookup = source
        .find(
            &LyricsQuery::new("$uicideboy$", "$UICIDE", None, Some(170)),
            &none(),
        )
        .await;

    assert!(lookup.result.is_none());
    assert!(!http.calls().await.iter().any(|c| c.contains("/download")));
}

#[tokio::test]
async fn kugou_search_keeps_a_dozen_distinct_entries_newest_search_first() {
    let entries: Vec<String> = (0..14)
        .map(|n| candidate(&n.to_string(), "Other", "Someone", 100_000))
        .chain(std::iter::once(candidate("0", "Dup", "Someone", 100_000)))
        .collect();
    let (_http, source) = FakeKugou {
        search_body: Some(format!(
            r#"{{"status":200,"candidates":[{}]}}"#,
            entries.join(",")
        )),
        ..FakeKugou::default()
    }
    .start()
    .await;

    let search = source
        .search(&LyricsQuery::new("Someone", "Song", None, Some(200)), &none())
        .await;

    assert!(!search.transient);
    assert_eq!(search.candidates.len(), 12);
    assert_eq!(search.candidates[0].id, "0.KEY0");
    assert_eq!(search.candidates[0].title, "Other");
    assert_eq!(search.candidates[0].duration_seconds, Some(100));
}
