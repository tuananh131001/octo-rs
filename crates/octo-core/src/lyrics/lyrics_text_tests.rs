//! `LyricsText` tests: the enhanced-LRC half of `LyricsWordTimingTests.cs`, the LyricsText half of
//! `LyricsTests.cs`, and `LyricsChoiceTests.Sidecar_OctosMark_IsSkippedByTheLrcReader`.

use super::*;
use crate::lyrics::lyrics_models::LyricsTiming;
use crate::lyrics::song_lyrics::OCTO_MARK;

fn starts(line: &LyricLine) -> Vec<i64> {
    line.words.iter().map(|word| word.start_ms).collect()
}

fn word_texts(line: &LyricLine) -> Vec<&str> {
    line.words.iter().map(|word| line.word_text(word)).collect()
}

// ---- LyricsWordTimingTests: enhanced LRC -------------------------------------------------

#[test]
fn a_word_before_a_pause_keeps_its_own_end() {
    // "Hold" ends at 1.40 s and "on" starts at 2.00 s: without its own end, "Hold" would
    // fill across the whole pause.
    let text = "Hold on";
    let line = LyricLine {
        words: vec![
            LyricWord::new(1000, Some(1400), 0, 5),
            LyricWord::new(2000, Some(2300), 5, 7),
        ],
        end_ms: Some(2300),
        ..LyricLine::new(1000, text)
    };

    let lrc = LyricsText::write_lrc([&line]);
    assert_eq!(lrc, "[00:01.00]<00:01.00>Hold <00:01.40><00:02.00>on<00:02.30>");

    let back = &LyricsText::parse_lrc(&lrc)[0];
    assert_eq!(back.text, text);
    assert_eq!(back.words[0].end_ms, Some(1400));
    assert_eq!(back.words[1].start_ms, 2000);
    assert_eq!(back.end_ms, Some(2300));

    // Words that run into each other get no extra tag.
    let joined = LyricLine {
        words: vec![
            LyricWord::new(1000, Some(1990), 0, 5),
            LyricWord::new(2000, Some(2300), 5, 7),
        ],
        end_ms: Some(2300),
        ..LyricLine::new(1000, text)
    };
    assert_eq!(
        LyricsText::write_lrc([&joined]),
        "[00:01.00]<00:01.00>Hold <00:02.00>on<00:02.30>"
    );
}

#[test]
fn enhanced_lrc_word_tags_become_words_and_never_reach_the_text() {
    let lines = LyricsText::parse_lrc(
        "[ar:x]\n[00:01.00]<00:01.00>Hello <00:01.50>world<00:02.00>\n[00:03.00]plain line",
    );

    assert_eq!(lines[0].text, "Hello world");
    assert_eq!(
        lines[0]
            .words
            .iter()
            .map(|word| (word.start_ms, word.end_ms, word.from, word.to))
            .collect::<Vec<_>>(),
        vec![(1000, Some(1500), 0, 6), (1500, Some(2000), 6, 11)]
    );
    assert_eq!(lines[0].end_ms, Some(2000));
    assert!(lines[1].words.is_empty());
    assert_eq!(lines[1].text, "plain line");
}

#[test]
fn enhanced_lrc_text_before_the_first_tag_starts_with_the_line() {
    let line = &LyricsText::parse_lrc("[00:05.00]Oh <00:05.40>yeah")[0];
    assert_eq!(line.text, "Oh yeah");
    assert_eq!(starts(line), vec![5000, 5400]);
}

#[test]
fn enhanced_lrc_a_line_sung_twice_carries_its_words_to_both() {
    let lines = LyricsText::parse_lrc("[00:10.00][00:30.00]<00:10.00>la <00:10.50>la");
    assert_eq!(starts(&lines[0]), vec![10_000, 10_500]);
    assert_eq!(starts(&lines[1]), vec![30_000, 30_500]);
}

#[test]
fn enhanced_lrc_write_then_read_is_the_same() {
    for text in ["Café naïve 日本", "🎵 la la", "It's  spaced   out"] {
        let pieces: Vec<&str> = text.split(' ').filter(|piece| !piece.is_empty()).collect();
        let joined = pieces.join(" ");
        let mut words = Vec::new();
        let mut at = 0;
        for (index, piece) in pieces.iter().enumerate() {
            let to = at + piece.len() + usize::from(index + 1 < pieces.len());
            let index = index as i64;
            words.push(LyricWord::new(
                61_230 + index * 400,
                Some(61_230 + (index + 1) * 400),
                at,
                to,
            ));
            at = to;
        }
        let line = LyricLine {
            end_ms: words.last().and_then(|word| word.end_ms),
            words: words.clone(),
            ..LyricLine::new(61_230, joined.clone())
        };

        let back = &LyricsText::parse_lrc(&LyricsText::write_lrc([&line]))[0];

        assert_eq!(back.text, joined, "{text}");
        assert_eq!(
            back.words
                .iter()
                .map(|word| (word.from, word.to))
                .collect::<Vec<_>>(),
            words.iter().map(|word| (word.from, word.to)).collect::<Vec<_>>(),
            "{text}"
        );
        assert_eq!(
            starts(back),
            words.iter().map(|word| word.start_ms).collect::<Vec<_>>(),
            "{text}"
        );
        assert_eq!(back.end_ms, line.end_ms, "{text}");
    }
}

#[test]
fn enhanced_lrc_has_word_tags_only_when_words_are_timed() {
    assert!(LyricsText::has_word_tags(Some("[00:01.00]<00:01.00>a")));
    assert!(!LyricsText::has_word_tags(Some("[00:01.00]a <b> c")));
    let result = |synced: Option<&str>, plain: Option<&str>| {
        LyricsResult::new("x", synced.map(String::from), plain.map(String::from), false).timing()
    };
    assert_eq!(result(Some("[00:01.00]<00:01.00>a"), None), LyricsTiming::Word);
    assert_eq!(result(Some("[00:01.00]a"), None), LyricsTiming::Line);
    assert_eq!(result(None, Some("a")), LyricsTiming::Plain);
}

#[test]
fn strip_credits_drops_headings_and_credits_the_evaluation_found() {
    for line in [
        "[00:01.00][Intro: Drake & PARTYNEXTDOOR]",
        "[00:01.00]Producers：Frank Dukes/Boi-1da",
        "[00:01.00]Writers：Kanye West",
        "[00:01.00]Artist: Skillet",
    ] {
        assert_eq!(
            LyricsText::strip_credits(&format!("{line}\n[00:40.00]sung")),
            "[00:40.00]sung",
            "{line}"
        );
    }
}

/// The Krc_BecomesEnhancedLrcThatReadsBackTheSame expectations, from the lines KuGou's parser
/// gives (the parser itself is ported with the KuGou source).
#[test]
fn krc_lines_become_enhanced_lrc_that_reads_back_the_same() {
    let text = "Work it make it";
    let first = LyricLine {
        words: vec![
            LyricWord::new(456, Some(942), 0, 5),
            LyricWord::new(942, Some(1157), 5, 8),
            LyricWord::new(1157, Some(1418), 8, 13),
            LyricWord::new(1418, Some(1596), 13, 15),
        ],
        end_ms: Some(1596),
        ..LyricLine::new(456, text)
    };

    assert_eq!(word_texts(&first), ["Work ", "it ", "make ", "it"]);
    let lrc = LyricsText::write_lrc([&first]);

    assert!(lrc.starts_with("[00:00.45]<00:00.45>Work <00:00.94>it <00:01.15>make <00:01.41>it<00:01.59>"));
    let back = LyricsText::parse_lrc(&lrc);
    assert_eq!(back[0].text, "Work it make it");
    assert_eq!(starts(&back[0]), vec![450, 940, 1150, 1410]);
    assert_eq!(back[0].end_ms, Some(1590));
}

// ---- LyricsTests: LyricsText -------------------------------------------------------------

#[test]
fn parse_lrc_reads_every_tag_and_fraction_and_sorts() {
    let lines =
        LyricsText::parse_lrc("[ar:Someone]\n[00:12.5]second\n[00:01.25][00:30.125]both\n[01:02]third");

    assert_eq!(
        lines.iter().map(|line| line.start_ms).collect::<Vec<_>>(),
        vec![1250, 12500, 30125, 62000]
    );
    assert_eq!(
        lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>(),
        vec!["both", "second", "both", "third"]
    );
}

/// The credit block from a real NetEase lyric (Jay Chou), as captured live 2026-09-25.
#[test]
fn strip_netease_credits_drops_the_leading_credit_block() {
    let lrc = [
        "[00:00.000] 作词 : 周杰伦",
        "[00:01.000] 作曲 : 周杰伦",
        "[00:02.000] 编曲 : 林迈可",
        "[00:03.00] 制作人 : 周杰伦",
        "[00:04.00]词版权管理方：杰威尔",
        "[00:20.50]窗外的麻雀 在电线杆上多嘴",
        "[00:24.00]你说这一句 很有夏天的感觉",
    ]
    .join("\n");

    let clean = LyricsText::strip_netease_credits(&lrc);

    assert!(!clean.contains("作词"));
    assert!(!clean.contains("编曲"));
    assert!(!clean.contains("版权"));
    assert!(clean.contains("窗外的麻雀"));
    assert!(clean.contains("很有夏天的感觉"));
}

#[test]
fn strip_netease_credits_drops_json_lines() {
    let lrc = "{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"唐恬\"}]}\n[00:10.00]first line";
    assert_eq!(LyricsText::strip_netease_credits(lrc), "[00:10.00]first line");
}

/// An English lyric that happens to have a colon is a lyric, even near the start.
#[test]
fn strip_netease_credits_keeps_an_english_lyric_with_a_colon() {
    for line in [
        "[00:05.00]Stop: don't you go",
        "[00:06.00]Baby: I'm yours",
        "[01:30.00]Hope: it's all we have",
    ] {
        assert_eq!(LyricsText::strip_netease_credits(line), line);
    }
}

#[test]
fn strip_netease_credits_drops_a_mid_song_publisher_line() {
    let lrc = "[00:12.00]a line\n[00:13.47]出品：网易飓风 X索尼音乐\n[00:15.00]another line";
    assert_eq!(
        LyricsText::strip_netease_credits(lrc),
        "[00:12.00]a line\n[00:15.00]another line"
    );
}

#[test]
fn query_title_keeps_the_version_but_not_the_guest_or_the_noise() {
    for (title, expected) in [
        ("Song (feat. Guest)", "Song"),
        ("Song (Live) [Official Video]", "Song (Live)"),
        ("Massive Attack - Teardrop", "Teardrop"),
    ] {
        assert_eq!(
            LyricsText::query_title(title, "Massive Attack"),
            expected,
            "{title}"
        );
    }
}

// ---- LyricsChoiceTests -------------------------------------------------------------------

#[test]
fn sidecar_octos_mark_is_skipped_by_the_lrc_reader() {
    let lines = LyricsText::parse_lrc(&format!("{OCTO_MARK}\n[00:01.00]<00:01.00>word"));
    assert_eq!(
        lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>(),
        ["word"]
    );
}

// ---- Rust-only: the .NET semantics this port had to keep ---------------------------------

/// `[^\s:：]{1,16}` counted UTF-16 units: a label of nine emoji is eighteen units, too long to
/// be a label, while eight are sixteen and still one.
#[test]
fn a_credit_label_counts_utf16_units() {
    let eight = "🎵".repeat(8);
    let nine = "🎵".repeat(9);
    assert_eq!(
        LyricsText::strip_credits(&format!("[00:01.00]{eight}: x\n[00:40.00]sung")),
        "[00:40.00]sung"
    );
    assert_eq!(
        LyricsText::strip_credits(&format!("[00:01.00]{nine}: x\n[00:40.00]sung")),
        format!("[00:01.00]{nine}: x\n[00:40.00]sung")
    );
}

/// .NET's invariant `IgnoreCase` takes `S` for `s` but not `ſ`, and the Kelvin sign for `k`.
#[test]
fn credit_words_ignore_case_as_dotnet_did() {
    assert!(LyricsText::is_credit_label("PRODUCERS"));
    assert!(!LyricsText::is_credit_label("ſamples"));
    assert!(LyricsText::is_credit_label("samples"));
    assert!(!LyricsText::is_credit_label("Stop"));
    assert_eq!(
        LyricsText::strip_credits("[00:01.00]trac\u{212A}: x\n[00:40.00]sung"),
        "[00:40.00]sung"
    );
}

#[test]
fn stamps_run_past_99_minutes_and_never_go_negative() {
    let line = LyricLine::new(6_000_000, "late");
    assert_eq!(LyricsText::write_lrc([&line]), "[100:00.00]late");
    assert_eq!(
        LyricsText::write_lrc([&LyricLine::new(-5, "early")]),
        "[00:00.00]early"
    );
}

#[test]
fn plain_text_and_preview_read_synced_lines_before_plain_text() {
    let synced = LyricsResult::new(
        "x",
        Some("[00:02.00]second\n[00:01.00]<00:01.00>first".into()),
        Some("ignored".into()),
        false,
    );
    assert_eq!(LyricsText::plain_text(&synced), "first\nsecond");
    assert_eq!(LyricsText::preview(&synced, 1), vec!["first"]);

    let plain = LyricsResult::new("x", None, Some("\r\n one \r\n\r\n two \r\n three".into()), false);
    assert_eq!(LyricsText::plain_text(&plain), "one \n\n two \n three");
    assert_eq!(LyricsText::preview(&plain, 2), vec!["one", "two"]);
}
