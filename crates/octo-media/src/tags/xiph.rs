//! A Vorbis comment as TagLib# (`TagLib.Ogg.XiphComment`) models it: the vendor string and a
//! .NET dictionary from upper-case field names to their values (whose enumeration order is the
//! order TagLib# writes them in). Pictures are its METADATA_BLOCK_PICTURE (or old COVERART)
//! fields, base64 text like any other value; pictures that are set are stored as those fields
//! when the comment is written, as TagLib# stores them when it renders.

use lofty::ogg::OggPictureStorage;
use lofty::ogg::tag::VorbisComments;
use lofty::picture::{Picture, PictureInformation};

use super::net::{parse_decimal_point_double, parse_int, parse_uint};
use super::net_dictionary::NetDictionary;
use octo_core::common::dotnet::to_upper_invariant;

pub(crate) const PICTURE_FIELD: &str = "METADATA_BLOCK_PICTURE";
const OLD_PICTURE_FIELD: &str = "COVERART";

#[derive(Debug, Clone, Default)]
pub(crate) struct XiphComment {
    pub(crate) vendor: String,
    pub(crate) fields: NetDictionary<Vec<String>>,
    /// `SaveBeatsPerMinuteAsTempo`: false once BPM was read from a BPM field.
    tempo_field: bool,
    /// Pictures set and not yet stored in the picture fields (`picture_fields_dirty`).
    pending_pictures: Option<Vec<Picture>>,
}

impl XiphComment {
    pub(crate) fn new() -> Self {
        Self {
            vendor: String::new(),
            fields: NetDictionary::default(),
            tempo_field: true,
            pending_pictures: None,
        }
    }

    /// The comment lofty read: its fields in file order, grouped by upper-case name as
    /// TagLib# groups them, and the pictures lofty took out of it as
    /// METADATA_BLOCK_PICTURE fields again.
    pub(crate) fn from_lofty(tag: &VorbisComments) -> Self {
        let mut comment = Self {
            vendor: tag.vendor().to_string(),
            ..Self::new()
        };
        for (key, value) in tag.items() {
            let key = to_upper_invariant(key);
            match comment.fields.get_mut(&key) {
                Some(values) => values.push(value.to_string()),
                // The first value goes through SetField, which drops a blank one.
                None => comment.set_field(&key, &[value]),
            }
        }
        let pictures: Vec<String> = tag
            .pictures()
            .iter()
            .map(|(picture, info)| encode_block(picture, *info))
            .collect();
        if !pictures.is_empty() {
            match comment.fields.get_mut(PICTURE_FIELD) {
                Some(values) => values.extend(pictures),
                None => comment.fields.insert(PICTURE_FIELD.to_string(), pictures),
            }
        }
        comment
    }

    /// The comment for lofty to write, field by field in this order. The picture fields go as
    /// they are, base64 text included.
    pub(crate) fn render(&mut self) -> VorbisComments {
        self.store_pictures();
        let mut tag = VorbisComments::new();
        tag.set_vendor(self.vendor.clone());
        for (key, values) in self.fields.iter() {
            for value in values {
                tag.push(key.clone(), value.clone());
            }
        }
        tag
    }

    /// `GetField`.
    pub(crate) fn field(&self, key: &str) -> Vec<String> {
        self.fields
            .get(&to_upper_invariant(key))
            .cloned()
            .unwrap_or_default()
    }

    /// `GetFirstField`.
    pub(crate) fn first_field(&self, key: &str) -> Option<String> {
        self.fields
            .get(&to_upper_invariant(key))
            .and_then(|values| values.first().cloned())
    }

    /// `SetField(key, values)`: blank values are dropped; none left removes the field. An
    /// existing field keeps its place.
    pub(crate) fn set_field(&mut self, key: &str, values: &[&str]) {
        let key = to_upper_invariant(key);
        let kept: Vec<String> = values
            .iter()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string())
            .collect();
        if kept.is_empty() {
            self.fields.remove(&key);
        } else {
            self.fields.insert(key.clone(), kept);
        }
        self.reset_pictures_state(&key);
    }

    /// `SetField(key, number, format)`: zero removes it.
    pub(crate) fn set_number_field(&mut self, key: &str, number: u32, width: usize) {
        if number == 0 {
            self.remove_field(key);
        } else {
            self.set_field(key, &[&format!("{number:0width$}")]);
        }
    }

    /// `RemoveField`.
    pub(crate) fn remove_field(&mut self, key: &str) {
        let key = to_upper_invariant(key);
        self.fields.remove(&key);
        self.reset_pictures_state(&key);
    }

    /// `ResetPicturesState`: writing a picture field directly drops pictures not yet stored.
    fn reset_pictures_state(&mut self, key: &str) {
        if key == PICTURE_FIELD || key == OLD_PICTURE_FIELD {
            self.pending_pictures = None;
        }
    }

    fn number_before_slash(&self, key: &str) -> u32 {
        self.first_field(key)
            .and_then(|text| text.split('/').next().and_then(parse_uint))
            .unwrap_or(0)
    }

    fn count(&self, total_key: &str, number_key: &str) -> u32 {
        if let Some(value) = self.first_field(total_key).and_then(|text| parse_uint(&text)) {
            return value;
        }
        self.first_field(number_key)
            .and_then(|text| text.split('/').nth(1).and_then(parse_uint))
            .unwrap_or(0)
    }

    pub(crate) fn track(&self) -> u32 {
        self.number_before_slash("TRACKNUMBER")
    }

    pub(crate) fn track_count(&self) -> u32 {
        self.count("TRACKTOTAL", "TRACKNUMBER")
    }

    pub(crate) fn disc(&self) -> u32 {
        self.number_before_slash("DISCNUMBER")
    }

    pub(crate) fn disc_count(&self) -> u32 {
        self.count("DISCTOTAL", "DISCNUMBER")
    }

    /// `Track` (set): the total is written again as it reads now, then the number as "00".
    pub(crate) fn set_track(&mut self, value: u32) {
        let count = self.track_count();
        self.set_number_field("TRACKTOTAL", count, 1);
        self.set_number_field("TRACKNUMBER", value, 2);
    }

    pub(crate) fn set_track_count(&mut self, value: u32) {
        self.set_number_field("TRACKTOTAL", value, 1);
    }

    pub(crate) fn set_disc(&mut self, value: u32) {
        let count = self.disc_count();
        self.set_number_field("DISCTOTAL", count, 1);
        self.set_number_field("DISCNUMBER", value, 1);
    }

    pub(crate) fn set_disc_count(&mut self, value: u32) {
        self.set_number_field("DISCTOTAL", value, 1);
    }

    /// `Year` (get): DATE's first four characters as a number.
    pub(crate) fn year(&self) -> u32 {
        self.first_field("DATE")
            .and_then(|text| {
                let head: String = if text.chars().count() > 4 {
                    text.chars().take(4).collect()
                } else {
                    text
                };
                parse_uint(&head)
            })
            .unwrap_or(0)
    }

    /// `BeatsPerMinute` (get): TEMPO, else BPM, rounded; reading BPM makes the setter write BPM.
    pub(crate) fn bpm(&mut self) -> u32 {
        self.tempo_field = true;
        let mut text = self.first_field("TEMPO").filter(|text| !text.is_empty());
        if text.is_none() {
            text = self.first_field("BPM").filter(|text| !text.is_empty());
            if text.is_some() {
                self.tempo_field = false;
            }
        }
        match text.and_then(|text| parse_decimal_point_double(&text)) {
            Some(value) if value > 0.0 => value.round_ties_even() as u32,
            _ => 0,
        }
    }

    pub(crate) fn set_bpm(&mut self, value: u32) {
        let key = if self.tempo_field { "TEMPO" } else { "BPM" };
        self.set_number_field(key, value, 1);
    }

    /// `AlbumArtists` (get): ALBUMARTIST, else "ALBUM ARTIST", else ENSEMBLE.
    pub(crate) fn album_artists(&self) -> Vec<String> {
        for key in ["ALBUMARTIST", "ALBUM ARTIST"] {
            let values = self.field(key);
            if !values.is_empty() {
                return values;
            }
        }
        self.field("ENSEMBLE")
    }

    /// `IsCompilation` (get): COMPILATION reads as the number 1.
    pub(crate) fn is_compilation(&self) -> bool {
        self.first_field("COMPILATION").and_then(|text| parse_int(&text)) == Some(1)
    }

    pub(crate) fn set_compilation(&mut self, value: bool) {
        if value {
            self.set_field("COMPILATION", &["1"]);
        } else {
            self.remove_field("COMPILATION");
        }
    }

    /// `Pictures` (get): the pictures set, else the old COVERART fields, then the
    /// METADATA_BLOCK_PICTURE ones.
    pub(crate) fn pictures(&self) -> Vec<(Picture, PictureInformation)> {
        if let Some(pending) = &self.pending_pictures {
            return pending
                .iter()
                .map(|picture| (picture.clone(), PictureInformation::default()))
                .collect();
        }
        let mut pictures = Vec::new();
        for value in self.field(OLD_PICTURE_FIELD) {
            if let Ok(data) = base64_decode(&value) {
                pictures.push((
                    Picture::unchecked(data)
                        .pic_type(lofty::picture::PictureType::Other)
                        .build(),
                    PictureInformation::default(),
                ));
            }
        }
        for value in self.field(PICTURE_FIELD) {
            if let Ok(picture) =
                Picture::from_flac_bytes(value.as_bytes(), true, lofty::config::ParsingMode::Relaxed)
            {
                pictures.push(picture);
            }
        }
        pictures
    }

    /// `Pictures` (set): kept aside until the comment is written.
    pub(crate) fn set_pictures(&mut self, pictures: &[Picture]) {
        self.pending_pictures = Some(pictures.to_vec());
    }

    /// `StorePictures`: the picture fields go, then one METADATA_BLOCK_PICTURE per picture, its
    /// sizes and colours zero as TagLib# writes them.
    fn store_pictures(&mut self) {
        let Some(pictures) = self.pending_pictures.take() else {
            return;
        };
        self.fields.remove(OLD_PICTURE_FIELD);
        self.fields.remove(PICTURE_FIELD);
        if !pictures.is_empty() {
            let blocks = pictures
                .iter()
                .map(|picture| encode_block(picture, PictureInformation::default()))
                .collect();
            self.fields.insert(PICTURE_FIELD.to_string(), blocks);
        }
    }
}

/// A FLAC picture block as base64 text.
fn encode_block(picture: &Picture, info: PictureInformation) -> String {
    String::from_utf8(picture.as_flac_bytes(info, true)).expect("base64 is ASCII")
}

fn base64_decode(text: &str) -> Result<Vec<u8>, ()> {
    // Picture::from_flac_bytes decodes base64 itself; COVERART needs the raw image.
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|c| *c == byte).ok_or(())? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_keep_taglibs_rules() {
        let mut xiph = XiphComment::new();
        xiph.set_field("title", &["  ", "A"]);
        xiph.set_field("ARTIST", &["B"]);
        xiph.set_field("TITLE", &["C"]);
        assert_eq!(xiph.fields.keys(), ["TITLE", "ARTIST"]);
        assert_eq!(xiph.field("title"), ["C"]);
        xiph.set_field("ARTIST", &[" "]);
        assert!(xiph.first_field("ARTIST").is_none());
    }

    #[test]
    fn track_and_disc_numbers_read_and_write_as_taglib_does() {
        let mut xiph = XiphComment::new();
        xiph.set_track(3);
        xiph.set_track_count(11);
        assert_eq!(xiph.field("TRACKNUMBER"), ["03"]);
        assert_eq!(xiph.field("TRACKTOTAL"), ["11"]);
        assert_eq!((xiph.track(), xiph.track_count()), (3, 11));
        xiph.set_field("DISCNUMBER", &["2/3"]);
        assert_eq!((xiph.disc(), xiph.disc_count()), (2, 3));
        xiph.set_field("DATE", &["1998-04-20"]);
        assert_eq!(xiph.year(), 1998);
    }

    #[test]
    fn base64_decodes() {
        assert_eq!(base64_decode("aGk="), Ok(b"hi".to_vec()));
    }
}
