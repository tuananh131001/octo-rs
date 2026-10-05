//! Port of `Services/Tagging/ReleaseDetails.cs`.
//!
//! The C# read a `JsonElement`; this reads a `serde_json::Value`. One difference in shape
//! handling is listed in `known-diffs.md`: a block of the wrong JSON kind where an object is
//! expected (an artist credit that is a string, say) made `TryGetProperty` throw; here it reads
//! as absent.

use indexmap::IndexSet;
use serde_json::Value;

use crate::common::dotnet::{eq_ignore_case, is_blank};
use crate::common::song_identity::SongIdentity;
use crate::tagging::net::cmp_ordinal;

/// One track of a release as the music database lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReleaseTrack {
    pub recording_id: Option<String>,
    pub release_track_id: Option<String>,
    pub position: Option<i32>,
    pub number: Option<String>,
    pub disc_number: i32,
    pub track_count: i32,
    pub title: Option<String>,
    pub length_seconds: Option<i32>,
    pub isrcs: Vec<String>,
}

/// A genre people voted on for a release, with the vote count.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReleaseGenre {
    pub name: String,
    pub count: i32,
}

/// What one music database release lookup adds to a candidate: its label and catalogue number,
/// barcode, status, country and date, its group's kind and first release date, the album
/// artist's ids, every track's position and ids, and the genres people voted on. A block the
/// answer lacks is None or empty, never a panic.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReleaseDetails {
    pub release_id: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub barcode: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub group_id: Option<String>,
    pub group_title: Option<String>,
    pub group_first_release_date: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub album_artist: Option<String>,
    pub album_artist_ids: Vec<String>,
    pub disc_count: i32,
    pub tracks: Vec<ReleaseTrack>,
    pub genres: Vec<ReleaseGenre>,
}

impl ReleaseDetails {
    pub fn is_compilation(&self) -> bool {
        self.secondary_types
            .iter()
            .any(|t| eq_ignore_case(t, "Compilation"))
            || self
                .album_artist
                .as_deref()
                .is_some_and(|a| eq_ignore_case(a, "Various Artists"))
    }

    /// The track that is this recording, or None when the release does not list it.
    pub fn track_for(&self, recording_id: Option<&str>) -> Option<&ReleaseTrack> {
        let recording_id = recording_id.filter(|id| !id.is_empty())?;
        self.tracks.iter().find(|t| {
            t.recording_id
                .as_deref()
                .is_some_and(|id| eq_ignore_case(id, recording_id))
        })
    }

    /// The genres with at least `min_votes` votes, most voted first (the C# defaults are 2 and 3).
    pub fn top_genres(&self, min_votes: i32, max: usize) -> Vec<String> {
        let mut genres: Vec<&ReleaseGenre> = self.genres.iter().filter(|g| g.count >= min_votes).collect();
        genres.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| cmp_ordinal(&a.name, &b.name)));
        genres.into_iter().take(max).map(|g| g.name.clone()).collect()
    }

    /// Read the release lookup's answer. None for a document that is not a release.
    pub fn parse(root: &Value) -> Option<ReleaseDetails> {
        if !root.is_object() {
            return None;
        }
        let id = str_of(root, "id").filter(|id| !id.is_empty())?;

        let (mut label, mut catalog_number) = (None, None);
        if let Some(labels) = root.get("label-info").and_then(Value::as_array) {
            for info in labels {
                if catalog_number.is_none() {
                    catalog_number = clean(str_of(info, "catalog-number"));
                }
                if let Some(l) = info.get("label").filter(|l| l.is_object())
                    && label.is_none()
                {
                    label = clean(str_of(l, "name"));
                }
                if label.is_some() && catalog_number.is_some() {
                    break;
                }
            }
        }

        let (mut group_id, mut group_title, mut group_first, mut primary_type) = (None, None, None, None);
        let mut secondary_types = Vec::new();
        if let Some(group) = root.get("release-group").filter(|g| g.is_object()) {
            group_id = str_of(group, "id");
            group_title = str_of(group, "title");
            group_first = clean(str_of(group, "first-release-date"));
            primary_type = str_of(group, "primary-type");
            if let Some(secondary) = group.get("secondary-types").and_then(Value::as_array) {
                secondary_types.extend(secondary.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }

        let (album_artist, album_artist_ids) = credits(root);

        let mut tracks = Vec::new();
        let mut disc_count = 0;
        if let Some(media) = root.get("media").and_then(Value::as_array) {
            for medium in media {
                disc_count += 1;
                let disc = int_of(medium, "position").unwrap_or(disc_count);
                let mut count = int_of(medium, "track-count").unwrap_or(0);
                let Some(list) = medium.get("tracks").and_then(Value::as_array) else {
                    continue;
                };
                if count == 0 {
                    count = i32::try_from(list.len()).unwrap_or(i32::MAX);
                }
                for track in list {
                    let (mut recording_id, mut title, mut length) = (None, None, None);
                    let mut isrcs = Vec::new();
                    if let Some(recording) = track.get("recording").filter(|r| r.is_object()) {
                        recording_id = str_of(recording, "id");
                        title = str_of(recording, "title");
                        isrcs = isrcs_of(recording);
                        if let Some(ms) = int_of(recording, "length") {
                            length = Some(seconds_of(ms));
                        }
                    }
                    if title.is_none() {
                        title = str_of(track, "title");
                    }
                    if length.is_none()
                        && let Some(ms) = int_of(track, "length")
                    {
                        length = Some(seconds_of(ms));
                    }
                    tracks.push(ReleaseTrack {
                        recording_id,
                        release_track_id: str_of(track, "id"),
                        position: int_of(track, "position"),
                        number: str_of(track, "number"),
                        disc_number: disc,
                        track_count: count,
                        title,
                        length_seconds: length,
                        isrcs,
                    });
                }
            }
        }

        let mut genres = Vec::new();
        if let Some(list) = root.get("genres").and_then(Value::as_array) {
            for genre in list {
                if let Some(name) = str_of(genre, "name").filter(|n| !n.is_empty()) {
                    genres.push(ReleaseGenre {
                        name,
                        count: int_of(genre, "count").unwrap_or(0),
                    });
                }
            }
        }

        Some(ReleaseDetails {
            release_id: id,
            title: str_of(root, "title"),
            status: clean(str_of(root, "status")),
            date: clean(str_of(root, "date")),
            country: clean(str_of(root, "country")),
            barcode: clean(str_of(root, "barcode")),
            label,
            catalog_number,
            group_id,
            group_title,
            group_first_release_date: group_first,
            primary_type,
            secondary_types,
            album_artist,
            album_artist_ids,
            disc_count,
            tracks,
            genres,
        })
    }
}

/// The release's artist credit as the database prints it, and each credited artist's id.
pub(crate) fn credits(element: &Value) -> (Option<String>, Vec<String>) {
    let Some(credit) = element.get("artist-credit").and_then(Value::as_array) else {
        return (None, Vec::new());
    };
    let mut text = String::new();
    let mut ids = Vec::new();
    for entry in credit {
        let mut name = str_of(entry, "name");
        if let Some(artist) = entry.get("artist").filter(|a| a.is_object()) {
            if name.is_none() {
                name = str_of(artist, "name");
            }
            if let Some(id) = str_of(artist, "id").filter(|id| !id.is_empty()) {
                ids.push(id);
            }
        }
        text.push_str(name.as_deref().unwrap_or(""));
        text.push_str(str_of(entry, "joinphrase").as_deref().unwrap_or(""));
    }
    let joined = text.trim();
    ((!joined.is_empty()).then(|| joined.to_string()), ids)
}

/// The recording's codes, normalised, each once, in the order listed.
pub(crate) fn isrcs_of(element: &Value) -> Vec<String> {
    let Some(isrcs) = element.get("isrcs").and_then(Value::as_array) else {
        return Vec::new();
    };
    isrcs
        .iter()
        .filter_map(Value::as_str)
        .filter_map(SongIdentity::normalize_isrc)
        .collect::<IndexSet<String>>()
        .into_iter()
        .collect()
}

fn clean(value: Option<String>) -> Option<String> {
    value.filter(|v| !is_blank(v)).map(|v| v.trim().to_string())
}

/// `(int)Math.Round(ms / 1000.0)`.
pub(crate) fn seconds_of(ms: i32) -> i32 {
    (f64::from(ms) / 1000.0).round_ties_even() as i32
}

/// The property when it is a JSON string (`JsonValueKind.String`).
pub(crate) fn str_of(element: &Value, name: &str) -> Option<String> {
    element.get(name).and_then(Value::as_str).map(str::to_string)
}

/// The property when it is a JSON number that fits an `int` (`TryGetInt32`): no fraction or
/// exponent, within range.
pub(crate) fn int_of(element: &Value, name: &str) -> Option<i32> {
    element
        .get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIGHT_AT_THE_OPERA: &str = r#"
    {
      "id": "r-nato", "title": "A Night at the Opera", "status": "Official", "date": "1975-11-21", "country": "GB",
      "barcode": "077778949224", "quality": "normal",
      "label-info": [{"catalog-number": "EMTC 103", "label": {"id": "l-emi", "name": "EMI"}}],
      "release-group": {"id": "g-nato", "title": "A Night at the Opera", "primary-type": "Album",
        "secondary-types": [], "first-release-date": "1975-11-21"},
      "artist-credit": [{"name": "Queen", "joinphrase": "", "artist": {"id": "a-queen", "name": "Queen"}}],
      "media": [{"position": 1, "format": "12\" Vinyl", "track-count": 12,
        "tracks": [
          {"id": "t-1", "position": 1, "number": "A1", "title": "Death on Two Legs", "length": 223000,
           "recording": {"id": "rec-dotl", "title": "Death on Two Legs (Dedicated to...)", "length": 223000, "isrcs": ["GBUM71029604"]}},
          {"id": "t-11", "position": 11, "number": "B5", "title": "Bohemian Rhapsody", "length": 355000,
           "recording": {"id": "rec-br", "title": "Bohemian Rhapsody", "length": 355000, "isrcs": ["GBUM71029604", "GBUM71029605"]}}
        ]}],
      "genres": [{"name": "rock", "count": 12}, {"name": "progressive rock", "count": 5}, {"name": "glam rock", "count": 1}]
    }
    "#;

    fn parse(text: &str) -> Option<ReleaseDetails> {
        ReleaseDetails::parse(&serde_json::from_str(text).expect("valid JSON"))
    }

    // ---- from MusicBrainzReleaseDetailsTests ----

    #[test]
    fn parse_reads_every_field() {
        let details = parse(NIGHT_AT_THE_OPERA).expect("a release");

        assert_eq!(details.release_id, "r-nato");
        assert_eq!(details.title.as_deref(), Some("A Night at the Opera"));
        assert_eq!(details.status.as_deref(), Some("Official"));
        assert_eq!(details.date.as_deref(), Some("1975-11-21"));
        assert_eq!(details.country.as_deref(), Some("GB"));
        assert_eq!(details.barcode.as_deref(), Some("077778949224"));
        assert_eq!(details.label.as_deref(), Some("EMI"));
        assert_eq!(details.catalog_number.as_deref(), Some("EMTC 103"));
        assert_eq!(details.group_id.as_deref(), Some("g-nato"));
        assert_eq!(details.group_first_release_date.as_deref(), Some("1975-11-21"));
        assert_eq!(details.primary_type.as_deref(), Some("Album"));
        assert!(details.secondary_types.is_empty());
        assert_eq!(details.album_artist.as_deref(), Some("Queen"));
        assert_eq!(details.album_artist_ids, ["a-queen"]);
        assert_eq!(details.disc_count, 1);
        assert!(!details.is_compilation());

        let track = details.track_for(Some("rec-br")).expect("the track");
        assert_eq!(track.release_track_id.as_deref(), Some("t-11"));
        assert_eq!(track.position, Some(11));
        assert_eq!(track.number.as_deref(), Some("B5"));
        assert_eq!(track.disc_number, 1);
        assert_eq!(track.track_count, 12);
        assert_eq!(track.length_seconds, Some(355));
        assert_eq!(track.isrcs, ["GBUM71029604", "GBUM71029605"]);
        assert!(details.track_for(Some("rec-other")).is_none());

        assert_eq!(details.top_genres(2, 3), ["rock", "progressive rock"]);
    }

    #[test]
    fn parse_missing_blocks_are_null_not_a_throw() {
        let details = parse(r#"{"id": "r-bare", "title": "Bare"}"#).expect("a release");

        assert_eq!(details.release_id, "r-bare");
        assert_eq!(details.label, None);
        assert_eq!(details.catalog_number, None);
        assert_eq!(details.barcode, None);
        assert_eq!(details.group_id, None);
        assert_eq!(details.status, None);
        assert!(details.tracks.is_empty());
        assert!(details.genres.is_empty());
        assert_eq!(details.disc_count, 0);
        assert!(details.top_genres(2, 3).is_empty());
    }

    #[test]
    fn parse_label_info_without_a_label_still_reads_the_catalog_number() {
        let details = parse(r#"{"id": "r", "label-info": [{"catalog-number": "CAT-1"}, {"label": {"name": "Later"}}], "barcode": ""}"#)
            .expect("a release");
        assert_eq!(details.catalog_number.as_deref(), Some("CAT-1"));
        assert_eq!(details.label.as_deref(), Some("Later"));
        assert_eq!(details.barcode, None);
    }

    #[test]
    fn parse_not_a_release_is_null() {
        assert_eq!(parse(r#"{"error": "Not Found"}"#), None);
    }

    #[test]
    fn parse_various_artists_group_is_a_compilation() {
        let details = parse(
            r#"
        {"id": "r-va", "artist-credit": [{"name": "Various Artists", "artist": {"id": "va", "name": "Various Artists"}}],
         "release-group": {"id": "g", "primary-type": "Album", "secondary-types": ["Compilation"]}}
        "#,
        )
        .expect("a release");
        assert!(details.is_compilation());
        assert_eq!(details.secondary_types, ["Compilation"]);
    }

    // ---- Rust-only: the JsonElement readings ----

    #[test]
    fn numbers_read_as_try_get_int32_and_lengths_round_to_even() {
        let value: Value =
            serde_json::from_str(r#"{"a": 1, "b": 1.0, "c": 1e3, "d": 3000000000, "e": "1"}"#).unwrap();
        assert_eq!(int_of(&value, "a"), Some(1));
        assert_eq!(int_of(&value, "b"), None);
        assert_eq!(int_of(&value, "c"), None);
        assert_eq!(int_of(&value, "d"), None);
        assert_eq!(int_of(&value, "e"), None);
        assert_eq!(seconds_of(2500), 2);
        assert_eq!(seconds_of(3500), 4);
    }

    #[test]
    fn a_medium_without_a_track_count_counts_its_tracks_and_genres_tie_by_name() {
        let details = parse(
            r#"{"id": "r", "media": [{"tracks": [{"id": "t1", "title": "One", "length": 1500}, {"id": "t2"}]}, {"position": 3}],
                "genres": [{"name": "b", "count": 4}, {"name": "a", "count": 4}, {"name": "", "count": 9}, {"name": "c"}]}"#,
        )
        .expect("a release");
        assert_eq!(details.disc_count, 2);
        assert_eq!(details.tracks.len(), 2);
        assert_eq!(details.tracks[0].track_count, 2);
        assert_eq!(details.tracks[0].disc_number, 1);
        assert_eq!(details.tracks[0].title.as_deref(), Some("One"));
        assert_eq!(details.tracks[0].length_seconds, Some(2));
        assert_eq!(details.top_genres(2, 3), ["a", "b"]);
        assert_eq!(details.top_genres(0, 3), ["a", "b", "c"]);
    }

    #[test]
    fn credits_join_with_their_phrases() {
        let value: Value = serde_json::from_str(
            r#"{"artist-credit": [{"name": "Massive Attack", "joinphrase": " feat. ", "artist": {"id": "a1"}},
                                   {"artist": {"id": "a2", "name": "Elizabeth Fraser"}}]}"#,
        )
        .unwrap();
        let (credit, ids) = credits(&value);
        assert_eq!(credit.as_deref(), Some("Massive Attack feat. Elizabeth Fraser"));
        assert_eq!(ids, ["a1", "a2"]);
        assert_eq!(credits(&serde_json::json!({})), (None, vec![]));
    }
}
