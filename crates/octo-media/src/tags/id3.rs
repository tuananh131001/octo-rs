//! An ID3v2 tag as TagLib# (`TagLib.Id3v2.Tag`) models it, kept as lofty frames.
//!
//! The frames stay in TagLib#'s order (a frame that is set keeps its place; a new one goes at
//! the end), and every text frame holds its values the way TagLib# reads them back: split at
//! the null separators, a version 3 tag's TPE1/TPE2/TCOM... split at "/" and its TCON's
//! "(17)" codes taken apart. Values are joined with "\0" in the lofty frame. When the tag is
//! written they are rendered as TagLib# renders them for the tag's version, so lofty's own
//! conversions find nothing left to convert.

use std::borrow::Cow;

use lofty::TextEncoding;
use lofty::id3::v2::{
    AttachedPictureFrame, ExtendedTextFrame, Frame, FrameId, Id3v2Tag, Id3v2Version, TextInformationFrame,
    UniqueFileIdentifierFrame, UnsynchronizedTextFrame,
};
use lofty::picture::Picture;

use super::genres;
use super::net::parse_byte;

/// The language TagLib# gives a lyrics frame it creates: `CultureInfo.CurrentCulture`'s
/// three-letter name, read once. The shipped image sets no LANG, so .NET runs with the
/// invariant culture, whose three-letter name is "ivl".
pub(crate) const LANGUAGE: [u8; 3] = *b"ivl";

/// TagLib#'s `Tag.DefaultVersion`: the version of a tag it creates.
pub(crate) const DEFAULT_VERSION: u8 = 3;

/// The frames TagLib# splits at "/" when it reads a version 3 (or 2) tag
/// (`TextInformationFrame.ParseRawData`).
const SLASH_SPLIT: [&str; 12] = [
    "TCOM", "TEXT", "TMCL", "TOLY", "TOPE", "TSOC", "TSOP", "TSO2", "TPE1", "TPE2", "TPE3", "TPE4",
];

/// An ID3v2 tag: its version (TagLib#'s `Version`, 3 or 4) and its frames.
#[derive(Debug, Clone)]
pub(crate) struct Id3Tag {
    pub(crate) version: u8,
    pub(crate) frames: Vec<Frame<'static>>,
}

/// One attached picture, as TagLib#'s `IPicture` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Id3Picture {
    pub(crate) picture: Picture,
    pub(crate) encoding: TextEncoding,
}

impl Id3Tag {
    /// A tag TagLib# creates: its default version, no frames.
    pub(crate) fn new() -> Self {
        Self {
            version: DEFAULT_VERSION,
            frames: Vec::new(),
        }
    }

    /// The tag lofty read, with its text frames' values as TagLib# would read them.
    pub(crate) fn from_lofty(tag: Id3v2Tag) -> Self {
        // TagLib# writes a version 2 tag back as version 2, which lofty cannot; it is kept as 3.
        let version = match tag.original_version() {
            Id3v2Version::V4 => 4,
            _ => 3,
        };
        let mut frames: Vec<Frame<'static>> = tag
            .into_iter()
            .filter_map(|frame| canonical(frame, version))
            .collect();
        if version < 4 {
            complete_recording_date(&mut frames);
        }
        Self { version, frames }
    }

    /// The tag to hand lofty, its frames rendered for `self.version`, and whether lofty must
    /// write it as version 3.
    pub(crate) fn to_lofty(&self) -> (Id3v2Tag, bool) {
        let mut tag = Id3v2Tag::new();
        for frame in &self.frames {
            if let Some(frame) = render(frame.clone(), self.version) {
                let _ = tag.insert(frame);
            }
        }
        (tag, self.version < 4)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn position_of_text(&self, id: &str) -> Option<usize> {
        self.frames
            .iter()
            .position(|frame| matches!(frame, Frame::Text(_)) && frame.id_str() == id)
    }

    /// `GetTextAsArray`: the first text frame with the id, as its values.
    pub(crate) fn text_values(&self, id: &str) -> Vec<String> {
        match self.position_of_text(id).map(|at| &self.frames[at]) {
            Some(Frame::Text(frame)) => split_values(&frame.value),
            _ => Vec::new(),
        }
    }

    /// Every text frame with the id, as its values in frame order (`GetFrames<TextInformationFrame>`).
    pub(crate) fn all_text_values(&self, id: &str) -> Vec<String> {
        self.frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Text(text) if frame.id_str() == id => Some(split_values(&text.value)),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// `GetTextAsString`: the first frame's values joined with "; ", null when that is empty.
    pub(crate) fn text_string(&self, id: &str) -> Option<String> {
        let joined = self.text_values(id).join("; ");
        (!joined.is_empty()).then_some(joined)
    }

    /// `GetTextAsUInt32`: the number before (index 0) or after (index 1) the "/".
    pub(crate) fn text_number(&self, id: &str, index: usize) -> u32 {
        let Some(text) = self.text_string(id) else {
            return 0;
        };
        let values: Vec<&str> = text.splitn(index + 2, '/').collect();
        values
            .get(index)
            .and_then(|value| super::net::parse_uint(value))
            .unwrap_or(0)
    }

    /// `SetTextFrame`: nothing but null or empty values removes every frame with the id;
    /// otherwise the first frame takes the values (in its place) in TagLib#'s default encoding.
    pub(crate) fn set_text_frame(&mut self, id: &str, values: &[&str]) {
        if values.iter().all(|value| value.is_empty()) {
            self.remove_frames(id);
            return;
        }
        let frame = Frame::Text(TextInformationFrame::new(
            frame_id(id),
            TextEncoding::UTF8,
            values.join("\0"),
        ));
        match self.position_of_text(id) {
            Some(at) => {
                let flags = self.frames[at].flags();
                self.frames[at] = frame;
                self.frames[at].set_flags(flags);
            }
            None => self.frames.push(frame),
        }
    }

    /// `SetNumberFrame`: "n" or "n/count", the number in the given minimum width ("00" is 2).
    pub(crate) fn set_number_frame(&mut self, id: &str, number: u32, count: u32, width: usize) {
        if number == 0 && count == 0 {
            self.remove_frames(id);
        } else if count != 0 {
            self.set_text_frame(id, &[&format!("{number:0width$}/{count}")]);
        } else {
            self.set_text_frame(id, &[&format!("{number:0width$}")]);
        }
    }

    /// `RemoveFrames(ident)`.
    pub(crate) fn remove_frames(&mut self, id: &str) {
        self.frames.retain(|frame| frame.id_str() != id);
    }

    fn position_of_user_text(&self, description: &str, case_sensitive: bool) -> Option<usize> {
        self.frames.iter().position(|frame| match frame {
            Frame::UserText(text) => {
                if case_sensitive {
                    text.description == description
                } else {
                    octo_core::common::dotnet::eq_ignore_case(&text.description, description)
                }
            }
            _ => false,
        })
    }

    /// `UserTextInformationFrame.Get(tag, description, false)?.Text`.
    pub(crate) fn user_text(&self, description: &str, case_sensitive: bool) -> Option<Vec<String>> {
        match self
            .position_of_user_text(description, case_sensitive)
            .map(|at| &self.frames[at])
        {
            Some(Frame::UserText(text)) => Some(split_values(&text.content)),
            _ => None,
        }
    }

    /// `UserTextInformationFrame.Get(tag, description, true).Text = values`: the frame keeps its
    /// place and its encoding; a new one is added at the end in TagLib#'s default encoding.
    pub(crate) fn set_user_text(&mut self, description: &str, values: &[String], case_sensitive: bool) {
        let content = values.join("\0");
        match self.position_of_user_text(description, case_sensitive) {
            Some(at) => {
                if let Frame::UserText(text) = &mut self.frames[at] {
                    text.content = Cow::Owned(content);
                }
            }
            None => self.frames.push(Frame::UserText(ExtendedTextFrame::new(
                TextEncoding::UTF8,
                description.to_string(),
                content,
            ))),
        }
    }

    /// `RemoveFrame` of the TXXX frame `Get(description, false)` finds.
    pub(crate) fn remove_user_text(&mut self, description: &str, case_sensitive: bool) {
        if let Some(at) = self.position_of_user_text(description, case_sensitive) {
            self.frames.remove(at);
        }
    }

    /// Every TXXX frame: its description and values, in frame order.
    pub(crate) fn user_texts(&self) -> Vec<(String, Vec<String>)> {
        self.frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::UserText(text) => Some((text.description.to_string(), split_values(&text.content))),
                _ => None,
            })
            .collect()
    }

    /// Removes the TXXX frames whose descriptions the predicate picks.
    pub(crate) fn retain_user_texts(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.frames.retain(|frame| match frame {
            Frame::UserText(text) => keep(&text.description),
            _ => true,
        });
    }

    /// `GetUserTextAsString`: the values joined with ";", null when that is empty.
    pub(crate) fn user_text_string(&self, description: &str, case_sensitive: bool) -> Option<String> {
        let joined = self.user_text(description, case_sensitive)?.join(";");
        (!joined.is_empty()).then_some(joined)
    }

    /// `SetUserTextAsString`: the text split at ";" (case-sensitive lookup); empty removes it.
    pub(crate) fn set_user_text_string(&mut self, description: &str, text: Option<&str>) {
        match text.filter(|text| !text.is_empty()) {
            Some(text) => {
                let values: Vec<String> = text.split(';').map(str::to_string).collect();
                self.set_user_text(description, &values, true);
            }
            None => self.remove_user_text(description, true),
        }
    }

    /// `UniqueFileIdentifierFrame.Get(tag, owner, false)?.Identifier`.
    pub(crate) fn ufid(&self, owner: &str) -> Option<&[u8]> {
        self.frames.iter().find_map(|frame| match frame {
            Frame::UniqueFileIdentifier(ufid) if ufid.owner == owner => Some(&*ufid.identifier),
            _ => None,
        })
    }

    /// `UniqueFileIdentifierFrame.Get(tag, owner, true).Identifier = identifier`.
    pub(crate) fn set_ufid(&mut self, owner: &str, identifier: &[u8]) {
        for frame in &mut self.frames {
            if let Frame::UniqueFileIdentifier(ufid) = frame
                && ufid.owner == owner
            {
                ufid.identifier = Cow::Owned(identifier.to_vec());
                return;
            }
        }
        self.frames
            .push(Frame::UniqueFileIdentifier(UniqueFileIdentifierFrame::new(
                owner.to_string(),
                identifier.to_vec(),
            )));
    }

    /// `UnsynchronisedLyricsFrame.GetPreferred(tag, "", Language)`: the frame in Octo's language
    /// with no description first, then one in its language, then one with no description,
    /// then the first.
    fn preferred_lyrics(&self) -> Option<usize> {
        let mut best: Option<(i32, usize)> = None;
        for (at, frame) in self.frames.iter().enumerate() {
            let Frame::UnsynchronizedText(uslt) = frame else {
                continue;
            };
            let same_name = uslt.description.is_empty();
            let same_lang = uslt.language == LANGUAGE;
            if same_name && same_lang {
                return Some(at);
            }
            let value = if same_lang {
                2
            } else if same_name {
                1
            } else {
                0
            };
            if best.is_none_or(|(best_value, _)| value > best_value) {
                best = Some((value, at));
            }
        }
        best.map(|(_, at)| at)
    }

    /// `Tag.Lyrics` (get).
    pub(crate) fn lyrics(&self) -> Option<String> {
        match self.preferred_lyrics().map(|at| &self.frames[at]) {
            Some(Frame::UnsynchronizedText(uslt)) => Some(uslt.content.to_string()),
            _ => None,
        }
    }

    /// `Tag.Lyrics` (set): nothing removes every frame the getter would find, one after another;
    /// text goes in the frame with no description in Octo's language, created when missing.
    pub(crate) fn set_lyrics(&mut self, lyrics: Option<&str>) {
        let Some(text) = lyrics.filter(|text| !text.is_empty()) else {
            while let Some(at) = self.preferred_lyrics() {
                self.frames.remove(at);
            }
            return;
        };
        for frame in &mut self.frames {
            if let Frame::UnsynchronizedText(uslt) = frame
                && uslt.description.is_empty()
                && uslt.language == LANGUAGE
            {
                uslt.content = Cow::Owned(text.to_string());
                uslt.encoding = TextEncoding::UTF8;
                return;
            }
        }
        self.frames
            .push(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
                TextEncoding::UTF8,
                LANGUAGE,
                String::new(),
                text.to_string(),
            )));
    }

    /// `Tag.Pictures` (get): the attached pictures in frame order.
    pub(crate) fn pictures(&self) -> Vec<Id3Picture> {
        self.frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Picture(apic) => Some(Id3Picture {
                    picture: apic.picture.clone().into_owned(),
                    encoding: apic.encoding,
                }),
                _ => None,
            })
            .collect()
    }

    /// `Tag.Pictures` (set): every APIC and GEOB frame goes, then one APIC per picture, in
    /// TagLib#'s default encoding.
    pub(crate) fn set_pictures(&mut self, pictures: &[Picture]) {
        self.remove_frames("APIC");
        self.remove_frames("GEOB");
        for picture in pictures {
            self.frames.push(Frame::Picture(AttachedPictureFrame::new(
                TextEncoding::UTF8,
                picture.clone(),
            )));
        }
    }

    /// `Tag.IsCompilation` (get): TCMP holds something other than "0".
    pub(crate) fn is_compilation(&self) -> bool {
        self.text_string("TCMP").is_some_and(|value| value != "0")
    }

    /// `Tag.IsCompilation` (set): "1", or no frame.
    pub(crate) fn set_compilation(&mut self, value: bool) {
        if value {
            self.set_text_frame("TCMP", &["1"]);
        } else {
            self.remove_frames("TCMP");
        }
    }
}

fn frame_id(id: &str) -> FrameId<'static> {
    FrameId::Valid(Cow::Owned(id.to_string()))
}

/// The values a joined frame holds: split at the nulls, a byte order mark that began a
/// value dropped (TagLib# reads one per value), and trailing empty values dropped
/// ("Bad tags may have one or more nul characters at the end of a string").
fn split_values(joined: &str) -> Vec<String> {
    let mut values: Vec<String> = joined
        .split('\0')
        .map(|value| value.strip_prefix('\u{FEFF}').unwrap_or(value).to_string())
        .collect();
    while values.last().is_some_and(String::is_empty) {
        values.pop();
    }
    values
}

/// A frame as lofty read it, put in this module's form: timestamps as text, and text frames
/// holding TagLib#'s values. None for a frame TagLib# would not keep (no data).
fn canonical(frame: Frame<'static>, version: u8) -> Option<Frame<'static>> {
    let flags = frame.flags();
    let mut frame = match frame {
        Frame::Timestamp(stamp) => {
            let id = stamp.id().as_str().to_string();
            let text = stamp.timestamp.to_string();
            Frame::Text(TextInformationFrame::new(frame_id(&id), stamp.encoding, text))
        }
        Frame::Text(text) => {
            // TagLib# files a version 3 TORY and TYER under their version 4 ids
            // (`FrameHeader.ConvertId`); lofty reads them unconverted.
            let id = match (version, text.id().as_str()) {
                (3, "TORY") => "TDOR".to_string(),
                (3, "TYER") => "TDRC".to_string(),
                (_, id) => id.to_string(),
            };
            let values = if version >= 4 {
                split_values(&text.value)
            } else {
                version3_values(&id, &text.value)
            };
            if values.is_empty() && text.value.is_empty() {
                return None;
            }
            Frame::Text(TextInformationFrame::new(
                frame_id(&id),
                text.encoding,
                values.join("\0"),
            ))
        }
        Frame::UserText(mut text) => {
            text.content = Cow::Owned(split_values(&text.content).join("\0"));
            Frame::UserText(text)
        }
        other => other,
    };
    frame.set_flags(flags);
    Some(frame)
}

/// `Tag.Parse`'s post-processing of a version 3 tag: the year in TDRC (TYER on disk) gets the
/// first TDAT's four digits as "-12-34" (TagLib# takes them in their written order), and the
/// first TIME's as "T12:34"; TDAT and TIME go once used.
fn complete_recording_date(frames: &mut Vec<Frame<'static>>) {
    let first = |frames: &[Frame<'static>], id: &str| {
        frames
            .iter()
            .position(|frame| matches!(frame, Frame::Text(_)) && frame.id_str() == id)
    };
    let text_of = |frame: &Frame<'static>| match frame {
        Frame::Text(text) => split_values(&text.value).join("; "),
        _ => String::new(),
    };
    let (Some(tdrc), Some(tdat)) = (first(frames, "TDRC"), first(frames, "TDAT")) else {
        return;
    };
    let year = text_of(&frames[tdrc]);
    if year.chars().count() != 4 {
        return;
    }
    let mut date = year;
    let tdat_text = text_of(&frames[tdat]);
    if tdat_text.chars().count() == 4 && tdat_text.is_ascii() {
        date.push_str(&format!("-{}-{}", &tdat_text[..2], &tdat_text[2..]));
        if let Some(time) = first(frames, "TIME") {
            let time_text = text_of(&frames[time]);
            if time_text.chars().count() == 4 && time_text.is_ascii() {
                date.push_str(&format!("T{}:{}", &time_text[..2], &time_text[2..]));
            }
            frames.retain(|frame| frame.id_str() != "TIME");
        }
    }
    frames.retain(|frame| frame.id_str() != "TDAT");
    if let Some(at) = first(frames, "TDRC")
        && let Frame::Text(text) = &mut frames[at]
    {
        text.value = Cow::Owned(date);
    }
}

/// `TextInformationFrame.ParseRawData` for a version 3 frame other than TXXX: the text up to
/// its first null, split at "/" for the people frames and taken apart for TCON.
fn version3_values(id: &str, raw: &str) -> Vec<String> {
    let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    let value = match raw.find('\0') {
        Some(0) => return Vec::new(),
        Some(at) => &raw[..at],
        None => raw,
    };
    let mut fields: Vec<String> = if SLASH_SPLIT.contains(&id) {
        value.split('/').map(str::to_string).collect()
    } else if id == "TCON" {
        tcon_version3_values(value)
    } else {
        vec![value.to_string()]
    };
    while fields.last().is_some_and(String::is_empty) {
        fields.pop();
    }
    fields
}

/// A version 3 TCON: "(17)" codes first, each one's own name after it skipped, then names
/// split at "/" and ";".
fn tcon_version3_values(text: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut value = text;
    while value.chars().count() > 1 && value.starts_with('(') {
        let Some(closing) = value.find(')') else {
            break;
        };
        let number = &value[1..closing];
        fields.push(number.to_string());
        value = value[closing + 1..].trim_start_matches(['/', ' ']);
        if let Some(name) = genres::text_to_audio(number)
            && let Some(rest) = value.strip_prefix(name)
        {
            value = rest.trim_start_matches(['/', ' ']);
        }
    }
    if !value.is_empty() {
        fields.extend(value.split(['/', ';']).map(str::to_string));
    }
    fields
}

/// A frame rendered for the tag's version the way TagLib# renders it.
fn render(frame: Frame<'static>, version: u8) -> Option<Frame<'static>> {
    let flags = frame.flags();
    let mut frame = match frame {
        Frame::Text(text) => {
            let id = text.id().as_str().to_string();
            let values = split_values(&text.value);
            // TagLib# writes a version 3 TDRC and TDOR under their version 3 names, as they are.
            let (id, value) = if version >= 4 {
                (id, join_values(&values, text.encoding, version))
            } else if id == "TCON" {
                (id, tcon_version3_text(&values))
            } else {
                let id = match id.as_str() {
                    "TDRC" => "TYER".to_string(),
                    "TDOR" => "TORY".to_string(),
                    _ => id,
                };
                (id, values.join("/"))
            };
            Frame::Text(TextInformationFrame::new(frame_id(&id), text.encoding, value))
        }
        Frame::UserText(mut text) => {
            text.content = Cow::Owned(join_values(&split_values(&text.content), text.encoding, version));
            Frame::UserText(text)
        }
        other => other,
    };
    frame.set_flags(flags);
    Some(frame)
}

/// Values joined with nulls. In UTF-16 every value has its own byte order mark, as TagLib#
/// writes them: U+FEFF after a separator encodes as the mark itself.
fn join_values(values: &[String], encoding: TextEncoding, version: u8) -> String {
    let utf16 = matches!(encoding, TextEncoding::UTF16) || (version < 4 && encoding == TextEncoding::UTF8);
    values.join(if utf16 { "\0\u{FEFF}" } else { "\0" })
}

/// `TextInformationFrame.RenderFields` for a version 3 TCON: numbers in parentheses while
/// every value so far was a number, then the rest joined with ";".
fn tcon_version3_text(values: &[String]) -> String {
    let mut data = String::new();
    let mut previous_indexed = true;
    for value in values {
        if !previous_indexed {
            data.push(';');
            data.push_str(value);
            continue;
        }
        match parse_byte(value) {
            Some(id) => {
                previous_indexed = true;
                data.push_str(&format!("({id})"));
            }
            None => {
                previous_indexed = false;
                data.push_str(value);
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version3_text_is_split_as_taglib_reads_it() {
        assert_eq!(version3_values("TPE2", "AC/DC"), vec!["AC", "DC"]);
        assert_eq!(version3_values("TIT2", "AC/DC"), vec!["AC/DC"]);
        assert_eq!(version3_values("TIT2", "one\0two"), vec!["one"]);
        assert_eq!(version3_values("TCON", "(17)Shoegaze"), vec!["17", "Shoegaze"]);
        assert_eq!(version3_values("TCON", "(17)Rock/Pop"), vec!["17", "Pop"]);
        assert_eq!(version3_values("TCON", "Trip Hop;17"), vec!["Trip Hop", "17"]);
    }

    #[test]
    fn version3_tcon_renders_as_taglib_renders_it() {
        let values = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(tcon_version3_text(&values(&["17", "Shoegaze"])), "(17)Shoegaze");
        assert_eq!(tcon_version3_text(&values(&["Trip Hop", "17"])), "Trip Hop;17");
        assert_eq!(tcon_version3_text(&values(&["17", "13"])), "(17)(13)");
    }

    #[test]
    fn values_lose_their_marks_and_trailing_blanks() {
        assert_eq!(split_values("a\0\u{FEFF}b\0\0"), vec!["a", "b"]);
        assert_eq!(
            join_values(&["a".into(), "b".into()], TextEncoding::UTF8, 3),
            "a\0\u{FEFF}b"
        );
        assert_eq!(
            join_values(&["a".into(), "b".into()], TextEncoding::UTF8, 4),
            "a\0b"
        );
        assert_eq!(
            join_values(&["a".into(), "b".into()], TextEncoding::Latin1, 3),
            "a\0b"
        );
    }
}
