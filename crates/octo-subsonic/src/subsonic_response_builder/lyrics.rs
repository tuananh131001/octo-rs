//! Port of `SubsonicResponseBuilder.Lyrics.cs`: getLyricsBySongId, getLyrics, and Octo's lyrics
//! choice answers.

use octo_core::lyrics::lyrics_choices::LyricsChoiceCandidate;
use octo_core::lyrics::lyrics_models::LyricsResult;
use octo_core::lyrics::lyrics_text::{LyricLine, LyricsText};
use serde_json::{Value, json};

use super::{Fields, SUBSONIC_VERSION, SubsonicReply, SubsonicResponseBuilder, envelope, subsonic_element};

pub const LYRICS_EXTENSION: &str = "octoLyrics";
pub const LYRICS_EXTENSION_VERSION: i32 = 1;

/// One word of a cue line: when it starts and ends, and where it sits in the line's value
/// in UTF-8 bytes, both ends included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueWord {
    pub start: i64,
    pub end: Option<i64>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub value: String,
}

/// A line with timed words, by its index among the lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueLineOut {
    pub index: usize,
    pub line: LyricLine,
    pub words: Vec<CueWord>,
}

impl SubsonicResponseBuilder {
    /// OpenSubsonic getLyricsBySongId (#52). Synced lyrics become timed lines in milliseconds,
    /// plain lyrics untimed lines; nothing, or an instrumental, is an empty but ok list, which
    /// is what stops a client logging "data not found" on every play.
    ///
    /// Word timing goes out only to a client that asked for it with enhanced=true (songLyrics
    /// version 2), as the spec has it: a kind, and a cueLine per timed line whose cues place
    /// each word in the line by UTF-8 bytes, both ends included. A client that did not ask gets
    /// exactly the lines it always got, and word tags never reach the text.
    pub fn create_lyrics_list_response(
        &self,
        format: &str,
        found: Option<&LyricsResult>,
        artist: &str,
        title: &str,
        enhanced: bool,
    ) -> SubsonicReply {
        let synced = found.filter(|f| f.has_synced());
        let timed: Vec<LyricLine> = synced
            .map(|f| LyricsText::parse_lrc(f.synced.as_deref().unwrap_or("")))
            .unwrap_or_default();
        let lines: Vec<(Option<i64>, String)> = match found {
            Some(f) if f.has_synced() => timed
                .iter()
                .map(|line| (Some(line.start_ms), line.text.clone()))
                .collect(),
            Some(f) if f.has_plain() => f
                .plain
                .as_deref()
                .unwrap_or("")
                .replace("\r\n", "\n")
                .split('\n')
                .map(|line| (None, line.trim().to_string()))
                .collect(),
            _ => Vec::new(),
        };
        let is_synced = synced.is_some();
        let cues = if enhanced { cue_lines(&timed) } else { Vec::new() };

        if format.eq_ignore_ascii_case("json") {
            let mut structured = Vec::new();
            if !lines.is_empty() {
                let mut entry = Fields::new();
                entry.insert("lang".into(), "xxx".into());
                entry.insert("synced".into(), is_synced.into());
                entry.insert("displayArtist".into(), artist.into());
                entry.insert("displayTitle".into(), title.into());
                entry.insert("offset".into(), 0.into());
                entry.insert(
                    "line".into(),
                    Value::Array(
                        lines
                            .iter()
                            .map(|(start, text)| match start {
                                Some(start) => json!({ "start": start, "value": text }),
                                None => json!({ "value": text }),
                            })
                            .collect(),
                    ),
                );
                if enhanced {
                    entry.insert("kind".into(), "main".into());
                    if !cues.is_empty() {
                        entry.insert(
                            "cueLine".into(),
                            Value::Array(cues.iter().map(cue_line_json).collect()),
                        );
                    }
                }
                structured.push(Value::Object(entry));
            }
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "lyricsList": { "structuredLyrics": structured },
            }));
        }

        let mut list = subsonic_element("lyricsList");
        if !lines.is_empty() {
            let mut element = subsonic_element("structuredLyrics")
                .attr("lang", "xxx")
                .attr("synced", if is_synced { "true" } else { "false" })
                .attr("displayArtist", artist)
                .attr("displayTitle", title)
                .attr("offset", 0)
                .children(lines.iter().map(|(start, text)| {
                    subsonic_element("line")
                        .attr_opt("start", *start)
                        .text(text.as_str())
                }));
            if enhanced {
                element.set_attr("kind", "main");
                for cue in &cues {
                    element.push(
                        subsonic_element("cueLine")
                            .attr("index", cue.index)
                            .attr("start", cue.line.start_ms)
                            .attr_opt("end", cue.line.end_ms)
                            .attr("value", cue.line.text.as_str())
                            .children(cue.words.iter().map(|word| {
                                subsonic_element("cue")
                                    .attr("start", word.start)
                                    .attr_opt("end", word.end)
                                    .attr("byteStart", word.byte_start)
                                    .attr("byteEnd", word.byte_end)
                                    .text(word.value.as_str())
                            })),
                    );
                }
            }
            list.push(element);
        }
        SubsonicReply::xml(&envelope("ok").child(list))
    }

    /// The legacy getLyrics answer: one block of untimed text, in the shape Navidrome gives, so
    /// a client that only knows artist and title still gets words. Nothing is an empty value.
    pub fn create_lyrics_response(
        &self,
        format: &str,
        found: Option<&LyricsResult>,
        artist: &str,
        title: &str,
    ) -> SubsonicReply {
        let text = match found {
            Some(f) if !f.instrumental => LyricsText::plain_text(f),
            _ => String::new(),
        };
        if format.eq_ignore_ascii_case("json") {
            let mut lyrics = Fields::new();
            lyrics.insert("value".into(), text.clone().into());
            if !text.is_empty() {
                lyrics.insert("artist".into(), artist.into());
                lyrics.insert("title".into(), title.into());
            }
            return self.create_json_response(json!({
                "status": "ok",
                "version": SUBSONIC_VERSION,
                "lyrics": lyrics,
            }));
        }

        let mut element = subsonic_element("lyrics").text(text.as_str());
        if !text.is_empty() {
            element.set_attr("artist", artist);
            element.set_attr("title", title);
        }
        SubsonicReply::xml(&envelope("ok").child(element))
    }

    /// getLyricsCandidates: every entry the sources hold for a song, for choosing
    /// between them, and what the song is set to (a candidate id, "none" or "auto").
    pub fn create_lyrics_candidates_response(
        &self,
        song_id: &str,
        choice: &str,
        candidates: &[LyricsChoiceCandidate],
    ) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "lyricsCandidates": {
                "id": song_id,
                "choice": choice,
                "candidate": candidates.iter().map(|candidate| json!({
                    "id": candidate.id,
                    "source": candidate.source,
                    "title": candidate.title,
                    "artist": candidate.artist,
                    "album": candidate.album,
                    "duration": candidate.duration_seconds,
                    "kind": candidate.kind,
                    "sameSong": candidate.same_song,
                    "chosen": candidate.id == choice,
                    "preview": candidate.preview,
                })).collect::<Vec<_>>(),
            },
        }))
    }

    pub fn create_lyrics_choice_response(&self, song_id: &str, choice: &str) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "lyricsChoice": { "id": song_id, "choice": choice },
        }))
    }
}

/// One cue line for each line with timed words, matched to it by index. A cue's place is
/// in UTF-8 bytes of the cue line's value, both ends included, so a client finds the word
/// the same way whatever alphabet it is written in.
///
/// (A word's range is already in UTF-8 bytes in Rust; C# counted the bytes of its UTF-16
/// range. A range that does not fall on characters of the text is left out, as one outside
/// the text was.)
pub fn cue_lines(lines: &[LyricLine]) -> Vec<CueLineOut> {
    let mut cue_lines = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.words.is_empty() {
            continue;
        }
        let words: Vec<CueWord> = line
            .words
            .iter()
            .filter(|word| word.to <= line.text.len() && word.to > word.from)
            .filter_map(|word| {
                let value = line.text.get(word.from..word.to)?;
                Some(CueWord {
                    start: word.start_ms,
                    end: word.end_ms,
                    byte_start: word.from,
                    byte_end: word.to - 1,
                    value: value.to_string(),
                })
            })
            .collect();
        if !words.is_empty() {
            cue_lines.push(CueLineOut {
                index,
                line: line.clone(),
                words,
            });
        }
    }
    cue_lines
}

fn cue_line_json(cue: &CueLineOut) -> Value {
    let mut json = Fields::new();
    json.insert("index".into(), cue.index.into());
    json.insert("start".into(), cue.line.start_ms.into());
    json.insert("value".into(), cue.line.text.clone().into());
    json.insert(
        "cue".into(),
        Value::Array(
            cue.words
                .iter()
                .map(|word| {
                    let mut entry = Fields::new();
                    entry.insert("start".into(), word.start.into());
                    entry.insert("byteStart".into(), word.byte_start.into());
                    entry.insert("byteEnd".into(), word.byte_end.into());
                    entry.insert("value".into(), word.value.clone().into());
                    if let Some(end) = word.end {
                        entry.insert("end".into(), end.into());
                    }
                    Value::Object(entry)
                })
                .collect(),
        ),
    );
    if let Some(line_end) = cue.line.end_ms {
        json.insert("end".into(), line_end.into());
    }
    Value::Object(json)
}
