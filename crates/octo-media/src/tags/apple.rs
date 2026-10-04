//! An iTunes item list as TagLib# (`TagLib.Mpeg4.AppleTag`) models it: boxes in file order,
//! each with its data boxes (a type code and raw bytes), the freeform "----" ones named by
//! their mean and name. Every value is kept as the bytes on disk, so what lofty writes back is
//! what TagLib# would have written.

use std::borrow::Cow;

use lofty::mp4::{Atom, AtomData, AtomIdent, DataType, Ilst};
use lofty::picture::{MimeType, Picture};

use octo_core::common::dotnet::eq_ignore_case;

/// TagLib#'s `AppleDataBox.FlagType`.
pub(crate) mod flags {
    pub(crate) const CONTAINS_DATA: u32 = 0;
    pub(crate) const CONTAINS_TEXT: u32 = 1;
    pub(crate) const CONTAINS_JPEG: u32 = 13;
    pub(crate) const CONTAINS_PNG: u32 = 14;
    pub(crate) const FOR_TEMPO: u32 = 21;
    pub(crate) const CONTAINS_BMP: u32 = 27;
}

/// The atom names TagLib#'s generic properties use (`BoxType`), 0xA9 for the copyright sign.
pub(crate) mod names {
    pub(crate) const NAM: [u8; 4] = *b"\xa9nam";
    pub(crate) const ART: [u8; 4] = *b"\xa9ART";
    pub(crate) const AART: [u8; 4] = *b"aART";
    pub(crate) const ALB: [u8; 4] = *b"\xa9alb";
    pub(crate) const WRT: [u8; 4] = *b"\xa9wrt";
    pub(crate) const GEN: [u8; 4] = *b"\xa9gen";
    pub(crate) const GNRE: [u8; 4] = *b"gnre";
    pub(crate) const DAY: [u8; 4] = *b"\xa9day";
    pub(crate) const TRKN: [u8; 4] = *b"trkn";
    pub(crate) const DISK: [u8; 4] = *b"disk";
    pub(crate) const LYR: [u8; 4] = *b"\xa9lyr";
    pub(crate) const TMPO: [u8; 4] = *b"tmpo";
    pub(crate) const CPRT: [u8; 4] = *b"cprt";
    pub(crate) const CPIL: [u8; 4] = *b"cpil";
    pub(crate) const COVR: [u8; 4] = *b"covr";
}

/// One `data` box: its type code (TagLib#'s flags) and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DataBox {
    pub(crate) flags: u32,
    pub(crate) data: Vec<u8>,
}

impl DataBox {
    fn text(text: &str) -> Self {
        Self {
            flags: flags::CONTAINS_TEXT,
            data: text.as_bytes().to_vec(),
        }
    }

    /// `AppleDataBox.Text`: the bytes as UTF-8 when the text flag bit is set.
    pub(crate) fn as_text(&self) -> Option<String> {
        (self.flags & flags::CONTAINS_TEXT != 0).then(|| String::from_utf8_lossy(&self.data).into_owned())
    }

    /// `ByteVector.ToUInt()`: up to the first four bytes, big-endian.
    fn as_uint(&self) -> u32 {
        self.data
            .iter()
            .take(4)
            .fold(0u32, |value, byte| (value << 8) | u32::from(*byte))
    }

    /// `ByteVector.Mid(at, 2).ToUShort()`.
    fn ushort_at(&self, at: usize) -> u32 {
        u32::from(self.data[at]) << 8 | u32::from(self.data[at + 1])
    }
}

/// One child of the item list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppleBox {
    Item {
        name: [u8; 4],
        data: Vec<DataBox>,
    },
    Dash {
        mean: String,
        name: String,
        data: Vec<DataBox>,
    },
}

impl AppleBox {
    fn data(&self) -> &[DataBox] {
        match self {
            AppleBox::Item { data, .. } | AppleBox::Dash { data, .. } => data,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AppleTag {
    pub(crate) boxes: Vec<AppleBox>,
}

impl AppleTag {
    /// The item list lofty read, each value back as its type code and bytes.
    pub(crate) fn from_lofty(ilst: &Ilst) -> Self {
        let boxes = ilst
            .into_iter()
            .map(|atom| {
                let data = atom.data().map(|value| data_box(atom.ident(), value)).collect();
                match atom.ident() {
                    AtomIdent::Fourcc(name) => AppleBox::Item { name: *name, data },
                    AtomIdent::Freeform { mean, name } => AppleBox::Dash {
                        mean: mean.to_string(),
                        name: name.to_string(),
                        data,
                    },
                }
            })
            .collect();
        Self { boxes }
    }

    /// The item list for lofty to write. A box with no data cannot be written; a box exactly
    /// like one before it (TagLib# adds a multi-value freeform box once per value) is written
    /// once, since lofty would merge the two.
    pub(crate) fn to_lofty(&self) -> Ilst {
        let mut ilst = Ilst::new();
        let mut written: Vec<&AppleBox> = Vec::new();
        for apple_box in &self.boxes {
            if written.contains(&apple_box) {
                continue;
            }
            written.push(apple_box);
            let ident = match apple_box {
                AppleBox::Item { name, .. } => AtomIdent::Fourcc(*name),
                AppleBox::Dash { mean, name, .. } => AtomIdent::Freeform {
                    mean: Cow::Owned(mean.clone()),
                    name: Cow::Owned(name.clone()),
                },
            };
            let data = apple_box
                .data()
                .iter()
                .map(|value| AtomData::Unknown {
                    code: DataType::from(value.flags),
                    data: value.data.clone(),
                })
                .collect();
            if let Some(atom) = Atom::from_collection(ident, data) {
                ilst.insert(atom);
            }
        }
        ilst
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// `DataBoxes(type)`: the data of every box with the name.
    fn data_boxes(&self, name: [u8; 4]) -> impl Iterator<Item = &DataBox> {
        self.boxes.iter().flat_map(move |apple_box| match apple_box {
            AppleBox::Item { name: own, data } if *own == name => data.as_slice(),
            _ => &[],
        })
    }

    /// `GetText`: every text value, split at ";" and trimmed.
    pub(crate) fn text(&self, name: [u8; 4]) -> Vec<String> {
        self.data_boxes(name)
            .filter_map(DataBox::as_text)
            .flat_map(|text| {
                text.split(';')
                    .map(|part| part.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The first data box's text (`foreach (box in DataBoxes) return box.Text`).
    pub(crate) fn first_text(&self, name: [u8; 4]) -> Option<String> {
        self.data_boxes(name).next().and_then(DataBox::as_text)
    }

    /// `SetText(type, string)`: nothing removes the box.
    pub(crate) fn set_text(&mut self, name: [u8; 4], text: Option<&str>) {
        match text.filter(|text| !text.is_empty()) {
            Some(text) => self.set_data(name, vec![DataBox::text(text)]),
            None => self.clear_data(name),
        }
    }

    /// `SetText(type, string[])`: the values joined with "; ".
    pub(crate) fn set_texts(&mut self, name: [u8; 4], values: &[String]) {
        self.set_text(name, Some(&values.join("; ")));
    }

    /// `SetData(type, boxes)`: the first box with the name takes the data and any later one is
    /// left empty; without one, a box is added at the end.
    pub(crate) fn set_data(&mut self, name: [u8; 4], boxes: Vec<DataBox>) {
        let mut boxes = Some(boxes);
        for apple_box in &mut self.boxes {
            if let AppleBox::Item { name: own, data } = apple_box
                && *own == name
            {
                *data = boxes.take().unwrap_or_default();
            }
        }
        if let Some(data) = boxes {
            self.boxes.push(AppleBox::Item { name, data });
        }
    }

    /// `ClearData(type)`: every box with the name goes.
    pub(crate) fn clear_data(&mut self, name: [u8; 4]) {
        self.boxes
            .retain(|apple_box| !matches!(apple_box, AppleBox::Item { name: own, .. } if *own == name));
    }

    /// The first freeform box with the mean and (ignoring case) the name.
    fn dash_position(&self, mean: &str, name: &str) -> Option<usize> {
        self.boxes.iter().position(|apple_box| {
            matches!(apple_box, AppleBox::Dash { mean: own_mean, name: own_name, .. }
                if own_mean == mean && eq_ignore_case(own_name, name))
        })
    }

    /// `GetDashBox`: the first value's text.
    pub(crate) fn dash_box(&self, mean: &str, name: &str) -> Option<String> {
        let at = self.dash_position(mean, name)?;
        self.boxes[at].data().first().and_then(DataBox::as_text)
    }

    /// `GetDashBoxes`: every value's text, None when there is no such box.
    pub(crate) fn dash_boxes(&self, mean: &str, name: &str) -> Option<Vec<Option<String>>> {
        let at = self.dash_position(mean, name)?;
        Some(self.boxes[at].data().iter().map(DataBox::as_text).collect())
    }

    /// `SetDashBox`: nothing removes the box; a value replaces the first value's bytes;
    /// otherwise a box is added at the end.
    pub(crate) fn set_dash_box(&mut self, mean: &str, name: &str, value: Option<&str>) {
        let found = self
            .dash_position(mean, name)
            .filter(|at| !self.boxes[*at].data().is_empty());
        match (found, value.filter(|value| !value.is_empty())) {
            (Some(at), None) => {
                self.boxes.remove(at);
            }
            (Some(at), Some(value)) => {
                if let AppleBox::Dash { data, .. } = &mut self.boxes[at] {
                    data[0].data = value.as_bytes().to_vec();
                }
            }
            (None, None) => {}
            (None, Some(value)) => self.boxes.push(AppleBox::Dash {
                mean: mean.to_string(),
                name: name.to_string(),
                data: vec![DataBox::text(value)],
            }),
        }
    }

    /// `SetDashBoxes`: an empty first value removes the box; as many values as the box holds
    /// replace them in place; otherwise the box is replaced by a new one at the end, holding
    /// one data box per value.
    pub(crate) fn set_dash_boxes(&mut self, mean: &str, name: &str, values: &[String]) {
        let found = self.dash_position(mean, name);
        if found.is_some() && values.first().is_none_or(String::is_empty) {
            if let Some(at) = found {
                self.boxes.remove(at);
            }
            return;
        }
        if let Some(at) = found {
            if let AppleBox::Dash { data, .. } = &mut self.boxes[at]
                && data.len() == values.len()
            {
                for (data_box, value) in data.iter_mut().zip(values) {
                    data_box.data = value.as_bytes().to_vec();
                }
                return;
            }
            self.boxes.remove(at);
        }
        self.boxes.push(AppleBox::Dash {
            mean: mean.to_string(),
            name: name.to_string(),
            data: values.iter().map(|value| DataBox::text(value)).collect(),
        });
    }

    /// Every freeform box with the mean: its name and its values' text.
    pub(crate) fn freeform(&self, mean: &str) -> Vec<(String, Vec<String>)> {
        self.boxes
            .iter()
            .filter_map(|apple_box| match apple_box {
                AppleBox::Dash {
                    mean: own,
                    name,
                    data,
                } if own == mean && !name.is_empty() => Some((
                    name.clone(),
                    data.iter()
                        .map(|value| value.as_text().unwrap_or_default())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// `Genres` (get): ©gen, else the old numeric gnre (the ID3v1 index plus one).
    pub(crate) fn genres(&self) -> Vec<String> {
        let text = self.text(names::GEN);
        if !text.is_empty() {
            return text;
        }
        for value in self.data_boxes(names::GNRE) {
            if value.flags != flags::CONTAINS_DATA || value.data.len() < 2 {
                continue;
            }
            let index = value.ushort_at(0);
            if index == 0 {
                continue;
            }
            if let Some(name) = super::genres::index_to_audio(((index - 1) & 0xFF) as u8) {
                return vec![name.to_string()];
            }
        }
        Vec::new()
    }

    pub(crate) fn set_genres(&mut self, values: &[String]) {
        self.clear_data(names::GNRE);
        self.set_texts(names::GEN, values);
    }

    /// `Year` (get): ©day as a number, else its first four characters as one.
    pub(crate) fn year(&self) -> u32 {
        for value in self.data_boxes(names::DAY) {
            if let Some(text) = value.as_text() {
                if let Some(year) = super::net::parse_uint(&text) {
                    return year;
                }
                let head: String = text.chars().take(4).collect();
                if let Some(year) = super::net::parse_uint(&head) {
                    return year;
                }
            }
        }
        0
    }

    pub(crate) fn set_year(&mut self, year: u32) {
        if year == 0 {
            self.clear_data(names::DAY);
        } else {
            self.set_text(names::DAY, Some(&year.to_string()));
        }
    }

    /// The number (at 2) or count (at 4) of trkn or disk.
    fn pair_value(&self, name: [u8; 4], at: usize) -> u32 {
        self.data_boxes(name)
            .find(|value| value.flags == flags::CONTAINS_DATA && value.data.len() >= at + 2)
            .map_or(0, |value| value.ushort_at(at))
    }

    fn set_pair(&mut self, name: [u8; 4], number: u32, count: u32) {
        if number == 0 && count == 0 {
            self.clear_data(name);
            return;
        }
        let mut data = vec![0, 0];
        data.extend_from_slice(&(number as u16).to_be_bytes());
        data.extend_from_slice(&(count as u16).to_be_bytes());
        data.extend_from_slice(&[0, 0]);
        self.set_data(
            name,
            vec![DataBox {
                flags: flags::CONTAINS_DATA,
                data,
            }],
        );
    }

    pub(crate) fn track(&self) -> u32 {
        self.pair_value(names::TRKN, 2)
    }

    pub(crate) fn track_count(&self) -> u32 {
        self.pair_value(names::TRKN, 4)
    }

    pub(crate) fn disc(&self) -> u32 {
        self.pair_value(names::DISK, 2)
    }

    pub(crate) fn disc_count(&self) -> u32 {
        self.pair_value(names::DISK, 4)
    }

    pub(crate) fn set_track(&mut self, value: u32) {
        let count = self.track_count();
        self.set_pair(names::TRKN, value, count);
    }

    pub(crate) fn set_track_count(&mut self, value: u32) {
        let track = self.track();
        self.set_pair(names::TRKN, track, value);
    }

    pub(crate) fn set_disc(&mut self, value: u32) {
        let count = self.disc_count();
        self.set_pair(names::DISK, value, count);
    }

    pub(crate) fn set_disc_count(&mut self, value: u32) {
        let disc = self.disc();
        self.set_pair(names::DISK, disc, value);
    }

    /// `BeatsPerMinute` (get): a tmpo box with the tempo flag, as a number.
    pub(crate) fn bpm(&self) -> u32 {
        self.data_boxes(names::TMPO)
            .find(|value| value.flags == flags::FOR_TEMPO)
            .map_or(0, DataBox::as_uint)
    }

    /// `BeatsPerMinute` (set): two bytes with the tempo flag; zero removes it.
    pub(crate) fn set_bpm(&mut self, value: u32) {
        if value == 0 {
            self.clear_data(names::TMPO);
        } else {
            let data = (value as u16).to_be_bytes().to_vec();
            self.set_data(
                names::TMPO,
                vec![DataBox {
                    flags: flags::FOR_TEMPO,
                    data,
                }],
            );
        }
    }

    /// `IsCompilation` (get): the first cpil value is not zero.
    pub(crate) fn is_compilation(&self) -> bool {
        self.data_boxes(names::CPIL)
            .next()
            .is_some_and(|value| value.as_uint() != 0)
    }

    /// `IsCompilation` (set): one byte, 1 or 0, with the tempo flag.
    pub(crate) fn set_compilation(&mut self, value: bool) {
        self.set_data(
            names::CPIL,
            vec![DataBox {
                flags: flags::FOR_TEMPO,
                data: vec![u8::from(value)],
            }],
        );
    }

    /// `Pictures` (get): every covr value.
    pub(crate) fn pictures(&self) -> Vec<Vec<u8>> {
        self.data_boxes(names::COVR)
            .map(|value| value.data.clone())
            .collect()
    }

    /// `Pictures` (set): one covr value per picture, typed by its MIME type; none removes covr.
    pub(crate) fn set_pictures(&mut self, pictures: &[Picture]) {
        if pictures.is_empty() {
            self.clear_data(names::COVR);
            return;
        }
        let boxes = pictures
            .iter()
            .map(|picture| {
                let flags = match picture.mime_type().map(MimeType::as_str) {
                    Some("image/jpeg") => flags::CONTAINS_JPEG,
                    Some("image/png") => flags::CONTAINS_PNG,
                    Some("image/x-windows-bmp") => flags::CONTAINS_BMP,
                    _ => flags::CONTAINS_DATA,
                };
                DataBox {
                    flags,
                    data: picture.data().to_vec(),
                }
            })
            .collect();
        self.set_data(names::COVR, boxes);
    }
}

/// One value lofty read, back as its type code and bytes.
fn data_box(ident: &AtomIdent<'_>, value: &AtomData) -> DataBox {
    let (flags, data) = match value {
        AtomData::UTF8(text) => (flags::CONTAINS_TEXT, text.as_bytes().to_vec()),
        AtomData::UTF16(text) => (2, text.encode_utf16().flat_map(u16::to_be_bytes).collect()),
        AtomData::Picture(picture) => {
            let flags = match picture.mime_type() {
                Some(MimeType::Jpeg) => flags::CONTAINS_JPEG,
                Some(MimeType::Png) => flags::CONTAINS_PNG,
                Some(MimeType::Gif) => 12,
                Some(MimeType::Bmp) => flags::CONTAINS_BMP,
                _ => flags::CONTAINS_DATA,
            };
            (flags, picture.data().to_vec())
        }
        // lofty reads a big-endian integer of up to four bytes as a number; TagLib# writes tmpo
        // in two bytes, and the rest in as few as hold the value.
        AtomData::SignedInteger(value) => {
            let width = if *ident == AtomIdent::Fourcc(names::TMPO) {
                2
            } else {
                int_width(i64::from(*value))
            };
            (flags::FOR_TEMPO, value.to_be_bytes()[4 - width..].to_vec())
        }
        AtomData::UnsignedInteger(value) => {
            let width = int_width(i64::from(*value));
            (22, value.to_be_bytes()[4 - width..].to_vec())
        }
        AtomData::Bool(value) => (flags::FOR_TEMPO, vec![u8::from(*value)]),
        AtomData::Unknown { code, data } => (u32::from(*code), data.clone()),
    };
    DataBox { flags, data }
}

fn int_width(value: i64) -> usize {
    if (-128..=255).contains(&value) {
        1
    } else if (-32768..=65535).contains(&value) {
        2
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_boxes_follow_taglibs_rules() {
        let mut apple = AppleTag::default();
        apple.set_dash_boxes("com.apple.iTunes", "ARTISTS", &["A".into(), "B".into()]);
        assert_eq!(
            apple.dash_boxes("com.apple.iTunes", "artists"),
            Some(vec![Some("A".into()), Some("B".into())])
        );
        apple.set_dash_box("com.apple.iTunes", "ARTISTS", Some("C"));
        assert_eq!(
            apple.dash_box("com.apple.iTunes", "ARTISTS").as_deref(),
            Some("C")
        );
        apple.set_dash_box("com.apple.iTunes", "ARTISTS", None);
        assert!(apple.is_empty());
    }

    #[test]
    fn numbers_are_stored_as_taglib_stores_them() {
        let mut apple = AppleTag::default();
        apple.set_track(3);
        apple.set_track_count(11);
        apple.set_bpm(77);
        apple.set_compilation(true);
        assert_eq!((apple.track(), apple.track_count(), apple.bpm()), (3, 11, 77));
        assert!(apple.is_compilation());
        assert_eq!(
            apple.boxes[0],
            AppleBox::Item {
                name: names::TRKN,
                data: vec![DataBox {
                    flags: 0,
                    data: vec![0, 0, 0, 3, 0, 11, 0, 0]
                }]
            }
        );
    }
}
