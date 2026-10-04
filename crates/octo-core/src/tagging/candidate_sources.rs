//! Port of `Services/Tagging/CandidateSources.cs`.

use std::collections::HashMap;

use serde_json::Value;

use crate::common::dotnet::{eq_ignore_case, is_blank, to_lower_invariant};
use crate::common::song_identity::SongIdentity;
use crate::fingerprint::acoust_id_client::{AcoustIdLookup, AcoustIdRecording, AcoustIdResult};
use crate::metadata::deezer_metadata_service::FullTrackMeta;
use crate::tagging::net::{cmp_ordinal, ignore_case_key, parse_int};
use crate::tagging::release_details::{credits, int_of, isrcs_of, seconds_of, str_of};
use crate::tagging::tag_evidence::{FileFacts, ReleaseCandidate, TagRequest, TagSource};

/// Turns each source's answer into candidates the chooser can weigh.
pub struct CandidateSources;

impl CandidateSources {
    /// One candidate per recording and release the fingerprint service named at or above
    /// the threshold. A recording with no release still counts, so the recording id can be written.
    pub fn from_fingerprint(lookup: Option<&AcoustIdLookup>, threshold: f64) -> Vec<ReleaseCandidate> {
        let Some(lookup) = lookup.filter(|l| l.is_ok) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for result in lookup.results.iter().filter(|r| r.score >= threshold) {
            for recording in &result.recordings {
                if recording.releases.is_empty() {
                    candidates.push(Self::base(recording, result));
                    continue;
                }
                for release in &recording.releases {
                    candidates.push(ReleaseCandidate {
                        release_id: release.release_id.clone(),
                        release_group_id: release.release_group_id.clone(),
                        release_title: release.title.clone(),
                        group_title: release.group_title.clone().or_else(|| release.title.clone()),
                        primary_type: release.primary_type.clone(),
                        secondary_types: release.secondary_types.clone(),
                        country: release.country.clone(),
                        release_date: release
                            .date
                            .clone()
                            .or_else(|| release.year.map(|y| y.to_string())),
                        track_number: release.track_number,
                        track_count: release.track_count,
                        disc_number: release.disc_number,
                        disc_count: release.disc_count,
                        album_artist: release.album_artist.clone(),
                        album_artist_ids: release.album_artist_ids.clone(),
                        release_track_id: release.release_track_id.clone(),
                        is_compilation: release.is_compilation,
                        ..Self::base(recording, result)
                    });
                }
            }
        }
        Self::with_group_first_dates(candidates)
    }

    fn base(recording: &AcoustIdRecording, result: &AcoustIdResult) -> ReleaseCandidate {
        ReleaseCandidate {
            recording_id: Some(recording.recording_id.clone()),
            primary_artist: recording.primary_artist().map(str::to_string),
            artists: recording.artists.clone(),
            artist_ids: recording
                .credits
                .iter()
                .filter_map(|c| c.artist_id.clone())
                .collect(),
            length_seconds: recording.duration_seconds,
            isrcs: recording.isrcs.clone(),
            fingerprint_id: result.id.clone(),
            sources: recording.sources,
            ..ReleaseCandidate::new(
                TagSource::Fingerprint,
                recording.title.clone(),
                recording.artist_credit(),
            )
        }
    }

    /// The fingerprint service gives each release its own date but not the group's first; the
    /// earliest date among a group's releases stands in for it. Only within what was returned,
    /// which is bounded, so a reissue-only answer reads as its own first.
    pub(crate) fn with_group_first_dates(candidates: Vec<ReleaseCandidate>) -> Vec<ReleaseCandidate> {
        let mut first: HashMap<String, String> = HashMap::new();
        for c in &candidates {
            let (Some(group), Some(date)) = (
                c.release_group_id.as_deref().filter(|g| !g.is_empty()),
                c.release_date.as_deref().filter(|d| !d.is_empty()),
            ) else {
                continue;
            };
            first
                .entry(ignore_case_key(group))
                .and_modify(|earliest| {
                    if cmp_ordinal(date, earliest).is_lt() {
                        *earliest = date.to_string();
                    }
                })
                .or_insert_with(|| date.to_string());
        }
        candidates
            .into_iter()
            .map(|c| {
                let date = match (&c.group_first_release_date, c.release_group_id.as_deref()) {
                    (None, Some(id)) if !id.is_empty() => first.get(&ignore_case_key(id)).cloned(),
                    _ => None,
                };
                match date {
                    Some(date) => ReleaseCandidate {
                        group_first_release_date: Some(date),
                        ..c
                    },
                    None => c,
                }
            })
            .collect()
    }

    /// One candidate from a catalog track, its album's kind read from the catalog's word for it.
    pub fn from_catalog(meta: &FullTrackMeta, requested_title: Option<&str>) -> ReleaseCandidate {
        let kind = meta.record_type.as_deref().map(|t| to_lower_invariant(t.trim()));
        let compilation =
            kind.as_deref() == Some("compile") || is_various_artists(meta.album_artist_name.as_deref());
        let date = meta
            .release_date
            .clone()
            .or_else(|| meta.year.map(|y| y.to_string()));
        let title = meta
            .title
            .clone()
            .or_else(|| requested_title.map(str::to_string))
            .unwrap_or_default();
        ReleaseCandidate {
            primary_artist: meta.artist_name.clone(),
            artists: meta.contributors.clone().unwrap_or_default(),
            length_seconds: meta.duration,
            isrcs: meta
                .isrc
                .as_deref()
                .and_then(SongIdentity::normalize_isrc)
                .into_iter()
                .collect(),
            release_title: meta.album_title.clone(),
            group_title: meta.album_title.clone(),
            primary_type: match kind.as_deref() {
                Some("album" | "compile") => Some("Album".to_string()),
                Some("single") => Some("Single".to_string()),
                Some("ep") => Some("EP".to_string()),
                _ => None,
            },
            secondary_types: if compilation {
                vec!["Compilation".to_string()]
            } else {
                Vec::new()
            },
            release_date: date.clone(),
            group_first_release_date: date,
            barcode: meta.barcode.clone(),
            label: meta.label.clone(),
            track_number: meta.track_number,
            track_count: meta.total_tracks,
            disc_number: meta.disc_number,
            album_artist: meta.album_artist_name.clone(),
            is_compilation: compilation,
            cover_url: meta.album_cover_url.clone(),
            genre: meta.genre.clone(),
            explicit: meta.explicit_lyrics,
            catalog_album_id: meta.album_id.clone(),
            catalog_track_id: meta.track_id.clone(),
            ..ReleaseCandidate::new(
                TagSource::Catalog,
                title,
                meta.artist_name.clone().unwrap_or_default(),
            )
        }
    }

    /// One candidate from what a peer tagged the file with. The title and artist fall
    /// back to the request's when the file names none, since the file's claim is its album.
    pub fn from_file_tags(file: &FileFacts, request: Option<&TagRequest>) -> Option<ReleaseCandidate> {
        if !file.tags_are_evidence || file.album.as_deref().is_none_or(is_blank) {
            return None;
        }
        let title = match file.title.as_deref() {
            Some(title) if !is_blank(title) => title.to_string(),
            _ => request.map(|r| r.title.clone()).unwrap_or_default(),
        };
        let artist = match file.artist.as_deref() {
            Some(artist) if !is_blank(artist) => artist.to_string(),
            _ => request.map(|r| r.artist.clone()).unwrap_or_default(),
        };
        Some(ReleaseCandidate {
            recording_id: file.recording_id.clone(),
            length_seconds: (file.duration_seconds > 0).then_some(file.duration_seconds),
            isrcs: file.isrcs.clone(),
            release_id: file.release_id.clone(),
            release_title: file.album.clone(),
            group_title: file.album.clone(),
            secondary_types: if file.is_compilation {
                vec!["Compilation".to_string()]
            } else {
                Vec::new()
            },
            release_date: file.year.map(|y| y.to_string()),
            barcode: file.barcode.clone(),
            label: file.label.clone(),
            catalog_number: file.catalog_number.clone(),
            track_number: file.track,
            disc_number: file.disc,
            album_artist: file.album_artist.clone(),
            is_compilation: file.is_compilation,
            ..ReleaseCandidate::new(TagSource::FileTags, title, artist)
        })
    }

    /// One candidate per recording and release in a music database recording search.
    pub fn from_database_search(root: &Value) -> Vec<ReleaseCandidate> {
        let Some(recordings) = root.get("recordings").and_then(Value::as_array) else {
            return Vec::new();
        };
        recordings
            .iter()
            .flat_map(|recording| Self::from_database_recording(recording, None))
            .collect()
    }

    /// One candidate per recording and release the music database lists for a code.
    pub fn from_isrc_lookup(root: &Value) -> Vec<ReleaseCandidate> {
        let Some(recordings) = root.get("recordings").and_then(Value::as_array) else {
            return Vec::new();
        };
        let isrc = str_of(root, "isrc").and_then(|code| SongIdentity::normalize_isrc(&code));
        recordings
            .iter()
            .flat_map(|recording| Self::from_database_recording(recording, isrc.as_deref()))
            .collect()
    }

    fn from_database_recording(recording: &Value, known_isrc: Option<&str>) -> Vec<ReleaseCandidate> {
        if recording.get("video") == Some(&Value::Bool(true)) {
            return Vec::new();
        }
        let id = str_of(recording, "id");
        let title = str_of(recording, "title").unwrap_or_default();
        // A live take or a remix says so in its disambiguation, not always in its title.
        let described = match str_of(recording, "disambiguation") {
            Some(disambiguation) if !is_blank(&disambiguation) => format!("{title} ({disambiguation})"),
            _ => title,
        };
        let (credit, artist_ids) = credits(recording);
        let artists = artist_names(recording);
        let length = int_of(recording, "length").map(seconds_of);
        let mut isrcs = isrcs_of(recording);
        if let Some(known) = known_isrc
            && !isrcs.iter().any(|isrc| isrc == known)
        {
            isrcs.push(known.to_string());
        }
        let first_release = str_of(recording, "first-release-date");

        let basis = ReleaseCandidate {
            recording_id: id,
            primary_artist: artists.first().cloned(),
            artists,
            artist_ids,
            length_seconds: length,
            isrcs,
            ..ReleaseCandidate::new(TagSource::Database, described, credit.unwrap_or_default())
        };

        let releases = match recording.get("releases").and_then(Value::as_array) {
            Some(releases) if !releases.is_empty() => releases,
            _ => {
                return vec![ReleaseCandidate {
                    group_first_release_date: first_release,
                    ..basis
                }];
            }
        };

        let mut candidates = Vec::new();
        for release in releases.iter().take(25) {
            let (mut group_id, mut group_title, mut group_first, mut primary_type) = (None, None, None, None);
            let mut secondary = Vec::new();
            if let Some(group) = release.get("release-group").filter(|g| g.is_object()) {
                group_id = str_of(group, "id");
                group_title = str_of(group, "title");
                group_first = str_of(group, "first-release-date");
                primary_type = str_of(group, "primary-type");
                if let Some(types) = group.get("secondary-types").and_then(Value::as_array) {
                    secondary.extend(types.iter().filter_map(Value::as_str).map(str::to_string));
                }
            }
            let (album_artist, album_artist_ids) = credits(release);

            let (mut track_number, mut track_count, mut disc_number, mut track_id) = (None, None, None, None);
            if let Some(media) = release.get("media").and_then(Value::as_array) {
                for medium in media {
                    let Some(track) = medium
                        .get("track")
                        .and_then(Value::as_array)
                        .and_then(|t| t.first())
                    else {
                        continue;
                    };
                    track_id = str_of(track, "id");
                    track_number = int_of(track, "position")
                        .or_else(|| str_of(track, "number").and_then(|number| parse_int(&number)));
                    if track_number.is_none()
                        && let Some(offset) = int_of(medium, "track-offset")
                    {
                        track_number = Some(offset.wrapping_add(1));
                    }
                    track_count = int_of(medium, "track-count");
                    disc_number = int_of(medium, "position");
                    break;
                }
            }

            let is_compilation = secondary.iter().any(|t| eq_ignore_case(t, "Compilation"))
                || album_artist
                    .as_deref()
                    .is_some_and(|a| eq_ignore_case(a, "Various Artists"));
            let release_title = str_of(release, "title");
            candidates.push(ReleaseCandidate {
                release_id: str_of(release, "id"),
                release_group_id: group_id,
                group_title: group_title.or_else(|| release_title.clone()),
                release_title,
                primary_type,
                secondary_types: secondary,
                status: str_of(release, "status"),
                country: str_of(release, "country"),
                release_date: blank(str_of(release, "date")),
                group_first_release_date: blank(group_first).or_else(|| blank(first_release.clone())),
                barcode: blank(str_of(release, "barcode")),
                track_number,
                track_count,
                disc_number,
                disc_count: release
                    .get("media")
                    .and_then(Value::as_array)
                    .map(|all| i32::try_from(all.len()).unwrap_or(i32::MAX)),
                album_artist,
                album_artist_ids,
                release_track_id: track_id,
                is_compilation,
                ..basis.clone()
            });
        }
        candidates
    }
}

fn artist_names(recording: &Value) -> Vec<String> {
    let Some(credit) = recording.get("artist-credit").and_then(Value::as_array) else {
        return Vec::new();
    };
    credit
        .iter()
        .filter_map(|entry| {
            str_of(entry, "name").or_else(|| entry.get("artist").and_then(|a| str_of(a, "name")))
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !is_blank(v))
}

/// "Various Artists", "Various" or "VA", ignoring case and surrounding space.
///
/// Port of `BaseDownloadService.IsVariousArtists` (the download base, task 4-B), which the
/// catalog candidate needs; that port can call this one.
pub fn is_various_artists(name: Option<&str>) -> bool {
    let value = name.unwrap_or("").trim();
    !value.is_empty()
        && ["Various Artists", "Various", "VA"]
            .iter()
            .any(|va| eq_ignore_case(value, va))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::acoust_id_client::{AcoustIdCredit, AcoustIdRelease};

    // ---- from MusicBrainzReleaseDetailsTests ----

    /// The code lookup's shape: recordings with a first-release-date and their releases.
    #[test]
    fn from_isrc_lookup_reads_recordings_and_their_releases() {
        let doc: Value = serde_json::from_str(
            r#"
        {"isrc": "GBDUW0000059", "recordings": [{
          "id": "rec-omt", "title": "One More Time", "length": 320000, "first-release-date": "2000-11-13",
          "artist-credit": [{"name": "Daft Punk", "artist": {"id": "a-dp", "name": "Daft Punk"}}],
          "releases": [
            {"id": "r-disc", "title": "Discovery", "status": "Official", "date": "2001-03-12", "country": "FR", "barcode": "724384960629",
             "release-group": {"id": "g-disc", "title": "Discovery", "primary-type": "Album", "first-release-date": "2001-02-26"},
             "artist-credit": [{"name": "Daft Punk", "artist": {"id": "a-dp", "name": "Daft Punk"}}],
             "media": [{"position": 1, "track-count": 14, "track-offset": 0, "track": [{"id": "t-omt", "number": "1", "position": 1, "title": "One More Time"}]}]},
            {"id": "r-single", "title": "One More Time", "status": "Official", "date": "2000-11-13", "country": "FR",
             "release-group": {"id": "g-single", "title": "One More Time", "primary-type": "Single", "first-release-date": "2000-11-13"}}
          ]}]}
        "#,
        )
        .expect("valid JSON");

        let candidates = CandidateSources::from_isrc_lookup(&doc);

        assert_eq!(candidates.len(), 2);
        let album = &candidates[0];
        assert_eq!(album.source, TagSource::Database);
        assert_eq!(album.recording_id.as_deref(), Some("rec-omt"));
        assert_eq!(album.isrcs, ["GBDUW0000059"]);
        assert_eq!(album.album_title(), Some("Discovery"));
        assert_eq!(album.release_group_id.as_deref(), Some("g-disc"));
        assert_eq!(album.group_first_release_date.as_deref(), Some("2001-02-26"));
        assert_eq!(album.barcode.as_deref(), Some("724384960629"));
        assert_eq!(album.track_number, Some(1));
        assert_eq!(album.track_count, Some(14));
        assert_eq!(album.release_track_id.as_deref(), Some("t-omt"));
        assert_eq!(album.album_artist_ids, ["a-dp"]);
        assert_eq!(candidates[1].primary_type.as_deref(), Some("Single"));
        assert_eq!(candidates[1].year(), Some(2000));
    }

    #[test]
    fn from_database_search_skips_videos_and_reads_a_recording_with_no_releases() {
        let doc: Value = serde_json::from_str(
            r#"
        {"recordings": [
          {"id": "v", "title": "Video", "video": true, "artist-credit": [{"name": "A"}]},
          {"id": "r", "title": "Song", "length": 200000, "first-release-date": "1999-01-01", "artist-credit": [{"name": "A", "artist": {"id": "a1", "name": "A"}}]}
        ]}
        "#,
        )
        .expect("valid JSON");
        let candidates = CandidateSources::from_database_search(&doc);
        assert_eq!(candidates.len(), 1);
        let only = &candidates[0];
        assert_eq!(only.recording_id.as_deref(), Some("r"));
        assert_eq!(only.release_id, None);
        assert_eq!(only.original_year(), Some(1999));
        assert_eq!(only.length_seconds, Some(200));
    }

    // ---- Rust-only ----

    #[test]
    fn from_fingerprint_fills_each_groups_first_date_from_its_earliest_release() {
        let release = |id: &str, group: &str, date: Option<&str>, year: Option<i32>| AcoustIdRelease {
            release_id: Some(id.into()),
            release_group_id: Some(group.into()),
            title: Some(format!("{group} title")),
            date: date.map(str::to_string),
            year,
            ..Default::default()
        };
        let recording = AcoustIdRecording {
            credits: vec![
                AcoustIdCredit::new("A", Some("a1"), ""),
                AcoustIdCredit::new("B", None, ""),
            ],
            releases: vec![
                release("r1", "g", Some("2019-01-01"), None),
                release("r2", "G", Some("1998-04-20"), None),
                release("r3", "other", None, Some(2001)),
            ],
            sources: 7,
            ..AcoustIdRecording::new("rec", "Song", ["A", "B"], None, None)
        };
        let bare = AcoustIdRecording::new("rec-bare", "Song", ["A"], None, None);
        let lookup = AcoustIdLookup::new(
            true,
            None,
            vec![
                AcoustIdResult {
                    id: Some("acoustid-1".into()),
                    ..AcoustIdResult::new(0.9, vec![recording, bare])
                },
                AcoustIdResult::new(
                    0.5,
                    vec![AcoustIdRecording::new("low", "Song", ["A"], None, None)],
                ),
            ],
        );

        let candidates = CandidateSources::from_fingerprint(Some(&lookup), 0.85);

        assert_eq!(candidates.len(), 4);
        assert_eq!(
            candidates[0].group_first_release_date.as_deref(),
            Some("1998-04-20")
        );
        assert_eq!(
            candidates[1].group_first_release_date.as_deref(),
            Some("1998-04-20")
        );
        assert_eq!(candidates[2].release_date.as_deref(), Some("2001"));
        assert_eq!(candidates[2].group_first_release_date.as_deref(), Some("2001"));
        assert_eq!(candidates[0].group_title.as_deref(), Some("g title"));
        assert_eq!(candidates[0].artist_credit, "A & B");
        assert_eq!(candidates[0].artist_ids, ["a1"]);
        assert_eq!(candidates[0].fingerprint_id.as_deref(), Some("acoustid-1"));
        assert_eq!(candidates[0].sources, 7);
        assert_eq!(candidates[3].recording_id.as_deref(), Some("rec-bare"));
        assert_eq!(candidates[3].release_id, None);

        assert!(CandidateSources::from_fingerprint(None, 0.85).is_empty());
        let refused = AcoustIdLookup::new(false, Some("bad key"), vec![]);
        assert!(CandidateSources::from_fingerprint(Some(&refused), 0.85).is_empty());
    }

    #[test]
    fn from_catalog_reads_the_kind_and_the_compilation() {
        let meta = FullTrackMeta {
            album_title: Some("Hits".into()),
            year: Some(2003),
            artist_name: Some("Artist".into()),
            isrc: Some("us-aaa-21-00001".into()),
            album_artist_name: Some(" various ".into()),
            record_type: Some(" Album ".into()),
            ..Default::default()
        };
        let candidate = CandidateSources::from_catalog(&meta, Some("Asked"));
        assert_eq!(candidate.source, TagSource::Catalog);
        assert_eq!(candidate.recording_title, "Asked");
        assert_eq!(candidate.primary_type.as_deref(), Some("Album"));
        assert_eq!(candidate.secondary_types, ["Compilation"]);
        assert!(candidate.is_compilation);
        assert_eq!(candidate.release_date.as_deref(), Some("2003"));
        assert_eq!(candidate.isrcs, ["USAAA2100001"]);

        let other = CandidateSources::from_catalog(
            &FullTrackMeta {
                record_type: Some("weird".into()),
                ..Default::default()
            },
            None,
        );
        assert_eq!(other.primary_type, None);
        assert_eq!(other.recording_title, "");
        assert!(!is_various_artists(Some("Various Artists Club")));
        assert!(is_various_artists(Some("va")));
        assert!(!is_various_artists(None));
    }

    #[test]
    fn from_file_tags_needs_evidence_and_an_album() {
        let request = TagRequest {
            artist: "Req Artist".into(),
            title: "Req Title".into(),
            ..Default::default()
        };
        let mut file = FileFacts {
            album: Some("Album".into()),
            title: Some(" ".into()),
            year: Some(1999),
            duration_seconds: 0,
            tags_are_evidence: true,
            ..Default::default()
        };
        let candidate = CandidateSources::from_file_tags(&file, Some(&request)).expect("a candidate");
        assert_eq!(candidate.recording_title, "Req Title");
        assert_eq!(candidate.artist_credit, "Req Artist");
        assert_eq!(candidate.release_date.as_deref(), Some("1999"));
        assert_eq!(candidate.length_seconds, None);

        file.tags_are_evidence = false;
        assert!(CandidateSources::from_file_tags(&file, Some(&request)).is_none());
        file.tags_are_evidence = true;
        file.album = Some("".into());
        assert!(CandidateSources::from_file_tags(&file, None).is_none());
    }

    #[test]
    fn a_database_track_without_a_position_reads_its_number_then_the_offset() {
        let doc: Value = serde_json::from_str(
            r#"{"recordings": [{"id": "r", "title": "Song", "disambiguation": "live",
                "releases": [
                  {"id": "a", "media": [{"track": []}, {"position": 2, "track-offset": 4, "track": [{"id": "t", "number": "x"}]}]},
                  {"id": "b", "date": " ", "media": [{"track": [{"number": "7"}]}]}
                ]}]}"#,
        )
        .expect("valid JSON");
        let candidates = CandidateSources::from_database_search(&doc);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].recording_title, "Song (live)");
        assert_eq!(candidates[0].track_number, Some(5));
        assert_eq!(candidates[0].disc_number, Some(2));
        assert_eq!(candidates[0].disc_count, Some(2));
        assert_eq!(candidates[1].track_number, Some(7));
        assert_eq!(candidates[1].release_date, None);
    }
}
