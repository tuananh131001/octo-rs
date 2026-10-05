//! The lyrics actions' statics: `LyricsSongSourceTests.NavidromeTiming_*` and `WithoutCues_*`.

use super::*;

fn answer(entries: &str) -> Vec<u8> {
    format!(r#"{{"subsonic-response":{{"status":"ok","lyricsList":{{"structuredLyrics":[{entries}]}}}}}}"#)
        .into_bytes()
}

#[test]
fn navidrome_timing_reads_cues_lines_and_plain() {
    assert_eq!(navidrome_lyrics_timing(&answer("")), Some(LyricsTiming::None));
    assert_eq!(
        navidrome_lyrics_timing(&answer(r#"{"synced":false,"line":[{"value":"a"}]}"#)),
        Some(LyricsTiming::Plain)
    );
    assert_eq!(
        navidrome_lyrics_timing(&answer(r#"{"synced":true,"line":[{"start":0,"value":"a"}]}"#)),
        Some(LyricsTiming::Line)
    );
    assert_eq!(
        navidrome_lyrics_timing(&answer(
            r#"{"synced":true,"kind":"main","line":[{"start":0,"value":"a"}],"cueLine":[{"index":0,"cue":[]}]}"#
        )),
        Some(LyricsTiming::Word)
    );
}

#[test]
fn without_cues_drops_the_cue_lines_and_kind_only() {
    let body = br#"{"subsonic-response":{"status":"ok","lyricsList":{"structuredLyrics":[{"synced":true,"kind":"main","line":[{"start":0,"value":"a"}],"cueLine":[{"index":0}]}]}}}"#;

    let text = String::from_utf8(without_cues(body)).expect("UTF-8");

    assert!(!text.contains("cueLine"), "{text}");
    assert!(!text.contains("kind"), "{text}");
    assert!(text.contains(r#""value":"a""#), "{text}");
}

/// Rust-only: what the C# read as unreadable (a non-object on the way, an entry that is not an
/// object) is `None`, and a body without lyrics at all is `LyricsTiming::None`; numbers and
/// escaping survive the rewrite as `JsonNode.ToJsonString` wrote them.
#[test]
fn navidrome_timing_and_without_cues_handle_odd_answers_as_the_csharp_did() {
    assert_eq!(navidrome_lyrics_timing(br#"{"subsonic-response":{}}"#), Some(LyricsTiming::None));
    assert_eq!(navidrome_lyrics_timing(br#"{"subsonic-response":7}"#), None);
    assert_eq!(navidrome_lyrics_timing(&answer("3")), None);
    assert_eq!(navidrome_lyrics_timing(br#"{"subsonic-response":{"lyricsList":{"structuredLyrics":{}}}}"#), None);
    assert_eq!(navidrome_lyrics_timing(b"not json"), None);

    let body = "{\"subsonic-response\":{\"lyricsList\":{\"structuredLyrics\":[{\"kind\":\"main\",\"offset\":1.50,\"line\":[{\"value\":\"caf\u{e9}\"}]}]}}}";
    let text = String::from_utf8(without_cues(body.as_bytes())).expect("UTF-8");
    assert_eq!(
        text,
        "{\"subsonic-response\":{\"lyricsList\":{\"structuredLyrics\":[{\"offset\":1.50,\"line\":[{\"value\":\"caf\\u00E9\"}]}]}}}"
    );
    // Nothing to remove: the body as it came.
    let untouched = br#"{"subsonic-response":{"lyricsList":{"structuredLyrics":[{"line":[]}]}}}"#;
    assert_eq!(without_cues(untouched), untouched.to_vec());
}
