//! Port of `Services/Soulseek/AlbumFolderPicker.cs`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::common::dotnet::{is_digit_utf16, utf16_class_view};
use crate::common::song_identity::SongIdentity;
use crate::soulseek::soulseek_client::SoulseekFileHit;
use crate::soulseek::soulseek_download_service::{
    adds_version, duration_plausible, filename_plausibly_matches_title, quality_penalty,
};

/// One track of an album, as the picker matches files to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTrack {
    pub external_id: String,
    pub title: String,
    pub duration: Option<i32>,
    pub number: Option<i32>,
}

impl AlbumTrack {
    pub fn new(
        external_id: impl Into<String>,
        title: impl Into<String>,
        duration: Option<i32>,
        number: Option<i32>,
    ) -> Self {
        AlbumTrack {
            external_id: external_id.into(),
            title: title.into(),
            duration,
            number,
        }
    }
}

/// The folder an album walk takes: whose it is, where, and the file each track gets, in
/// album order.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumFolderChoice {
    pub username: String,
    pub folder: String,
    pub files: Vec<(AlbumTrack, SoulseekFileHit)>,
}

/// Picks the one peer folder that covers most of an album, so the album comes from one rip in one
/// batch instead of a search, a peer and a queue per song.
///
/// Every check a song's own search makes applies to each file here too: the title as a phrase in
/// the file name, a length within the window, no version the track did not ask for. On top of that
/// a file's leading track number counts, and matching is global rather than in track order, so
/// "Hold On" never takes "Hold On, We're Going Home" when both are on the record.
pub struct AlbumFolderPicker;

/// Album mode needs a length for most tracks: the per-file length check is what keeps a
/// wrong file out, and a track without one goes to the song by song search instead.
pub const MIN_KNOWN_LENGTHS: f64 = 0.8;

/// A folder must cover at least this share of the tracks still wanted.
pub const MIN_COVERAGE: f64 = 0.5;

/// A peer with a free upload slot starts sending now. Its folder wins when it covers at
/// least this share of what the fullest folder covers.
pub const FREE_SLOT_SHARE: f64 = 0.75;

static DISC_FOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(cd|dis[ck])\s*\d+$").expect("a fixed pattern compiles"));

/// One candidate folder: whose, where, and its matched pairs.
struct Candidate<'a> {
    username: &'a str,
    folder: String,
    files: Vec<(&'a AlbumTrack, &'a SoulseekFileHit)>,
}

impl AlbumFolderPicker {
    pub fn choose(
        hits: &[SoulseekFileHit],
        tracks: &[AlbumTrack],
        usable: impl Fn(&SoulseekFileHit) -> bool,
    ) -> Option<AlbumFolderChoice> {
        if tracks.is_empty() {
            return None;
        }
        let known: Vec<&AlbumTrack> = tracks
            .iter()
            .filter(|t| t.duration.is_some_and(|d| d > 0))
            .collect();
        if (known.len() as f64) < MIN_KNOWN_LENGTHS * tracks.len() as f64 {
            return None;
        }

        // GroupBy keeps the groups in the order their first member appeared.
        let mut groups: Vec<((&str, String), Vec<&SoulseekFileHit>)> = Vec::new();
        let mut index: HashMap<(&str, String), usize> = HashMap::new();
        for hit in hits.iter().filter(|hit| usable(hit)) {
            let key = (hit.username.as_str(), Self::folder_of(&hit.filename));
            match index.get(&key) {
                Some(&at) => groups[at].1.push(hit),
                None => {
                    index.insert(key.clone(), groups.len());
                    groups.push((key, vec![hit]));
                }
            }
        }
        let folders: Vec<Candidate> = groups
            .into_iter()
            .map(|((username, folder), files)| Candidate {
                username,
                folder,
                files: Self::match_files(&known, &files),
            })
            .filter(|folder| !folder.files.is_empty())
            .collect();
        if folders.is_empty() {
            return None;
        }

        let fullest = folders.iter().map(|f| f.files.len()).max().unwrap_or(0);
        let average_quality = |f: &Candidate| -> f64 {
            f.files
                .iter()
                .map(|(_, file)| quality_penalty(file) as f64)
                .sum::<f64>()
                / f.files.len() as f64
        };
        let free_and_enough =
            |f: &Candidate| free_now(&f.files) && f.files.len() as f64 >= FREE_SLOT_SHARE * fullest as f64;
        // OrderByDescending(...).ThenByDescending(...).ThenBy(...): a stable sort, so the first
        // of equals is the first folder found.
        let mut order: Vec<usize> = (0..folders.len()).collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (&folders[a], &folders[b]);
            free_and_enough(y)
                .cmp(&free_and_enough(x))
                .then(y.files.len().cmp(&x.files.len()))
                .then(average_quality(x).total_cmp(&average_quality(y)))
                .then(
                    x.files[0]
                        .1
                        .queue_length
                        .unwrap_or(i32::MAX)
                        .cmp(&y.files[0].1.queue_length.unwrap_or(i32::MAX)),
                )
                .then(
                    y.files[0]
                        .1
                        .upload_speed
                        .unwrap_or(0)
                        .cmp(&x.files[0].1.upload_speed.unwrap_or(0)),
                )
        });
        let pick = &folders[order[0]];

        let needed = 2f64.max((MIN_COVERAGE * tracks.len() as f64).ceil());
        if (pick.files.len() as f64) < needed {
            return None;
        }
        let album_order: HashMap<&str, usize> = tracks
            .iter()
            .enumerate()
            .map(|(i, track)| (track.external_id.as_str(), i))
            .collect();
        let mut files: Vec<(AlbumTrack, SoulseekFileHit)> = pick
            .files
            .iter()
            .map(|(track, file)| ((*track).clone(), (*file).clone()))
            .collect();
        files.sort_by_key(|(track, _)| album_order[track.external_id.as_str()]);
        Some(AlbumFolderChoice {
            username: pick.username.to_string(),
            folder: pick.folder.clone(),
            files,
        })
    }

    /// The folder a file is in, with a CD1 or Disc 2 folder counted as its parent, so a
    /// two-disc rip is one candidate.
    pub fn folder_of(remote_filename: &str) -> String {
        let normalized = remote_filename.replace('\\', "/");
        let mut parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
        if !parts.is_empty() {
            parts.pop();
        }
        if parts.len() > 1
            && let Some(last) = parts.last()
            && DISC_FOLDER.is_match(&utf16_class_view(last.trim()))
        {
            parts.pop();
        }
        parts.join("/")
    }

    /// Every track and file pair that passes the filters, scored, then taken lowest score first,
    /// each track and each file once. Score: the title exactly (0) or only as a phrase (3); the
    /// file's leading number equal to the track's (0), missing (1) or different (4); plus the length
    /// difference in seconds.
    fn match_files<'a>(
        tracks: &[&'a AlbumTrack],
        files: &[&'a SoulseekFileHit],
    ) -> Vec<(&'a AlbumTrack, &'a SoulseekFileHit)> {
        // (track, file index, score)
        let mut pairs: Vec<(&AlbumTrack, usize, i32)> = Vec::new();
        for track in tracks {
            for (at, file) in files.iter().enumerate() {
                if !filename_plausibly_matches_title(&file.filename, &track.title, true) {
                    continue;
                }
                if !duration_plausible(file.length, track.duration, false) {
                    continue;
                }
                if adds_version(&file.filename, &track.title) {
                    continue;
                }
                let normalized = file.filename.replace('\\', "/");
                let leaf = file_name_without_extension(normalized.rsplit('/').next().unwrap_or(""));
                let number = leading_number(leaf).and_then(|(_, digits)| digits.parse::<i32>().ok());
                let score = (if exact_title(leaf, &track.title) { 0 } else { 3 })
                    + match (number, track.number) {
                        (None, _) | (_, None) => 1,
                        (Some(n), Some(t)) if n == t => 0,
                        _ => 4,
                    }
                    + match (file.length, track.duration) {
                        (Some(length), Some(duration)) => (length - duration).abs(),
                        _ => 4,
                    };
                pairs.push((track, at, score));
            }
        }

        // OrderBy(score).ThenBy(filename, Ordinal): stable, so ties keep track-then-file order.
        // Ordinal compares UTF-16 code units.
        pairs.sort_by(|a, b| {
            a.2.cmp(&b.2).then_with(|| {
                files[a.1]
                    .filename
                    .encode_utf16()
                    .cmp(files[b.1].filename.encode_utf16())
            })
        });
        let mut taken_tracks: HashSet<&str> = HashSet::new();
        let mut taken_files: HashSet<usize> = HashSet::new();
        let mut matched = Vec::new();
        for (track, at, _) in pairs {
            if taken_tracks.contains(track.external_id.as_str()) || taken_files.contains(&at) {
                continue;
            }
            taken_tracks.insert(&track.external_id);
            taken_files.insert(at);
            matched.push((track, files[at]));
        }
        matched
    }
}

fn free_now(files: &[(&AlbumTrack, &SoulseekFileHit)]) -> bool {
    files[0].1.has_free_upload_slot == Some(true)
}

/// `Path.GetFileNameWithoutExtension` of a name with no separator in it.
fn file_name_without_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(at) => &name[..at],
        None => name,
    }
}

/// "03 - Song", "03. Song", "1-03 Song": the track number, without a disc prefix. The C#
/// pattern, `^\s*(?:\d{1,2}\s*[-.]\s*(?=\d))?0*(\d{1,3})(?!\d)`, has lookarounds the `regex`
/// crate lacks, so it is matched by hand in the order the backtracking engine tries it.
/// Returns where the match ends (a byte offset) and the captured digits.
fn leading_number(text: &str) -> Option<(usize, &str)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let end_of = |i: usize| chars.get(i).map_or(text.len(), |&(at, _)| at);
    let digit = |i: usize| chars.get(i).is_some_and(|&(_, c)| is_digit_utf16(c));
    let space = |i: usize| chars.get(i).is_some_and(|&(_, c)| c.is_whitespace());
    let skip_space = |mut i: usize| {
        while space(i) {
            i += 1;
        }
        i
    };

    // 0*(\d{1,3})(?!\d) from `from`: the zeros greedy, then the digits greedy, each giving
    // back one at a time.
    let tail = |from: usize| -> Option<(usize, &str)> {
        let mut zeros = 0;
        while chars.get(from + zeros).is_some_and(|&(_, c)| c == '0') {
            zeros += 1;
        }
        for z in (0..=zeros).rev() {
            let start = from + z;
            for len in (1..=3).rev() {
                if (start..start + len).all(digit) && !digit(start + len) {
                    return Some((end_of(start + len), &text[end_of(start)..end_of(start + len)]));
                }
            }
        }
        None
    };

    let p = skip_space(0);
    // The optional disc prefix first, its digits greedy: \d{1,2}\s*[-.]\s*(?=\d).
    for k in (1..=2).rev() {
        if !(p..p + k).all(digit) {
            continue;
        }
        let mut q = skip_space(p + k);
        if !chars.get(q).is_some_and(|&(_, c)| c == '-' || c == '.') {
            continue;
        }
        q = skip_space(q + 1);
        if !digit(q) {
            continue;
        }
        if let Some(found) = tail(q) {
            return Some(found);
        }
    }
    tail(p)
}

/// The file name, once a leading number and an "Artist - " are taken off, is the title.
fn exact_title(leaf: &str, title: &str) -> bool {
    let wanted = SongIdentity::key(title);
    if wanted.is_empty() {
        return false;
    }
    let rest = match leading_number(leaf) {
        Some((end, _)) => &leaf[end..],
        None => leaf,
    }
    .trim_start_matches([' ', '-', '.', '_']);
    if SongIdentity::key(rest) == wanted {
        return true;
    }
    match rest.rfind(" - ") {
        Some(dash) => SongIdentity::key(&rest[dash + 3..]) == wanted,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    //! Port of the picker half of `octo.Tests/AlbumFolderTests.cs` (the walk through the
    //! download service belongs to 4-B and 4-C).
    //!
    //! An album heart used to search, pick a peer and queue song by song. These pin down
    //! choosing one peer's folder of the album.

    use super::*;

    fn t(number: i32, title: &str, seconds: i32) -> AlbumTrack {
        AlbumTrack::new(format!("id-{number}"), title, Some(seconds), Some(number))
    }

    fn f(user: &str, path: &str, seconds: i32, ext: &str, free: Option<bool>) -> SoulseekFileHit {
        SoulseekFileHit {
            username: user.into(),
            filename: path.into(),
            size: 30_000_000,
            length: Some(seconds),
            extension: ext.into(),
            has_free_upload_slot: free,
            queue_length: Some(0),
            upload_speed: Some(1000),
            ..Default::default()
        }
    }

    fn flac_only(hit: &SoulseekFileHit) -> bool {
        hit.extension == "flac"
    }

    fn album() -> Vec<AlbumTrack> {
        vec![
            t(1, "Intro", 90),
            t(2, "Hold On", 200),
            t(3, "Hold On, We're Going Home", 228),
            t(4, "Started", 180),
        ]
    }

    fn folder(user: &str, dir: &str, ext: &str, free: Option<bool>, skip: &[i32]) -> Vec<SoulseekFileHit> {
        album()
            .into_iter()
            .filter(|t| !skip.contains(&t.number.unwrap_or(0)))
            .map(|t| {
                f(
                    user,
                    &format!("{dir}\\{:02} - {}.{ext}", t.number.unwrap_or(0), t.title),
                    t.duration.unwrap_or(0),
                    ext,
                    free,
                )
            })
            .collect()
    }

    fn flac(user: &str, dir: &str, skip: &[i32]) -> Vec<SoulseekFileHit> {
        folder(user, dir, "flac", None, skip)
    }

    #[test]
    fn one_flac_folder_beats_a_fuller_mp3_folder() {
        let mut hits = folder("mp3peer", r"Music\Drake\Album", "mp3", None, &[]);
        hits.extend(folder("flacpeer", r"Music\Drake\Album", "flac", None, &[4]));
        let choice = AlbumFolderPicker::choose(&hits, &album(), flac_only).expect("a choice");
        assert_eq!(choice.username, "flacpeer");
        assert_eq!(choice.files.len(), 3);
    }

    #[test]
    fn a_folder_with_most_of_the_album_beats_the_album_spread_over_peers() {
        let mut hits = flac("whole", r"a\Album", &[4]);
        hits.extend(flac("p1", r"b\Album", &[2, 3, 4]));
        hits.extend(flac("p2", r"c\Album", &[1, 3, 4]));
        hits.extend(flac("p3", r"d\Album", &[1, 2, 4]));
        assert_eq!(
            AlbumFolderPicker::choose(&hits, &album(), flac_only)
                .expect("a choice")
                .username,
            "whole"
        );
    }

    #[test]
    fn hold_on_never_takes_hold_on_were_going_home() {
        let choice =
            AlbumFolderPicker::choose(&flac("peer", r"x\Album", &[]), &album(), flac_only).expect("a choice");
        let hold_on = &choice
            .files
            .iter()
            .find(|(t, _)| t.title == "Hold On")
            .expect("Hold On")
            .1;
        assert!(hold_on.filename.ends_with("02 - Hold On.flac"));
        let home = &choice
            .files
            .iter()
            .find(|(t, _)| t.title.starts_with("Hold On, "))
            .expect("Hold On, We're Going Home")
            .1;
        assert!(home.filename.ends_with("03 - Hold On, We're Going Home.flac"));
        let distinct: HashSet<&str> = choice.files.iter().map(|(_, f)| f.filename.as_str()).collect();
        assert_eq!(distinct.len(), 4);
    }

    #[test]
    fn a_file_of_the_wrong_length_is_not_matched() {
        let mut hits = flac("peer", r"x\Album", &[]);
        hits[3].length = Some(180 + 11);
        let choice = AlbumFolderPicker::choose(&hits, &album(), flac_only).expect("a choice");
        assert_eq!(choice.files.len(), 3);
        assert!(!choice.files.iter().any(|(t, _)| t.title == "Started"));
    }

    #[test]
    fn under_half_the_album_is_no_choice() {
        assert!(
            AlbumFolderPicker::choose(&flac("peer", r"x\Album", &[2, 3, 4]), &album(), flac_only).is_none()
        );
    }

    #[test]
    fn a_denied_file_is_passed_over() {
        let hits = flac("peer", r"x\Album", &[]);
        let choice = AlbumFolderPicker::choose(&hits, &album(), |hit| {
            flac_only(hit) && !hit.filename.contains("Intro")
        })
        .expect("a choice");
        assert!(!choice.files.iter().any(|(t, _)| t.title == "Intro"));
    }

    #[test]
    fn a_two_disc_rip_is_one_folder() {
        let hits = vec![
            f("peer", r"Music\Album\CD1\01 - Intro.flac", 90, "flac", None),
            f("peer", r"Music\Album\CD1\02 - Hold On.flac", 200, "flac", None),
            f(
                "peer",
                r"Music\Album\Disc 2\03 - Hold On, We're Going Home.flac",
                228,
                "flac",
                None,
            ),
            f("peer", r"Music\Album\Disc 2\04 - Started.flac", 180, "flac", None),
        ];
        let choice = AlbumFolderPicker::choose(&hits, &album(), flac_only).expect("a choice");
        assert_eq!(choice.files.len(), 4);
        assert_eq!(choice.folder, "Music/Album");
    }

    #[test]
    fn too_few_known_lengths_is_no_album_mode() {
        let unknown: Vec<AlbumTrack> = album()
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                if i < 2 {
                    AlbumTrack { duration: None, ..t }
                } else {
                    t
                }
            })
            .collect();
        assert!(AlbumFolderPicker::choose(&flac("peer", r"x\Album", &[]), &unknown, flac_only).is_none());
    }

    #[test]
    fn a_peer_with_a_free_slot_wins_when_it_covers_enough() {
        let mut hits = folder("queued", r"a\Album", "flac", Some(false), &[]);
        hits.extend(folder("free", r"b\Album", "flac", Some(true), &[4]));
        assert_eq!(
            AlbumFolderPicker::choose(&hits, &album(), flac_only)
                .expect("a choice")
                .username,
            "free"
        );

        let mut thin = folder("queued", r"a\Album", "flac", Some(false), &[]);
        thin.extend(folder("free", r"b\Album", "flac", Some(true), &[3, 4]));
        assert_eq!(
            AlbumFolderPicker::choose(&thin, &album(), flac_only)
                .expect("a choice")
                .username,
            "queued"
        );
    }

    #[test]
    fn files_come_back_in_album_order() {
        let mut hits = flac("peer", r"x\Album", &[]);
        hits.reverse();
        let choice = AlbumFolderPicker::choose(&hits, &album(), flac_only).expect("a choice");
        let titles: Vec<&str> = choice.files.iter().map(|(t, _)| t.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Intro", "Hold On", "Hold On, We're Going Home", "Started"]
        );
    }

    #[test]
    fn the_leading_number_reads_as_the_csharp_pattern_did() {
        let cases = [
            ("03 - Song", Some("3")),
            ("03. Song", Some("3")),
            ("1-03 Song", Some("3")),
            ("2-08 Massive Attack", Some("8")),
            ("  007 Bond", Some("7")),
            ("000", Some("0")),
            ("1000 Words", None),
            ("Song", None),
            ("12.5 Song", Some("5")),
            ("123", Some("123")),
        ];
        for (text, expected) in cases {
            assert_eq!(leading_number(text).map(|(_, d)| d), expected, "{text:?}");
        }
        assert_eq!(leading_number("03 - Song").map(|(end, _)| end), Some(2));
        assert!(exact_title("03 - Hold On", "Hold On"));
        assert!(exact_title("Drake - Hold On", "Hold On"));
        assert!(!exact_title("03 - Hold On, We're Going Home", "Hold On"));
    }

    #[test]
    fn a_disc_folder_counts_as_its_parent_but_a_lone_one_does_not() {
        assert_eq!(AlbumFolderPicker::folder_of(r"A\Album\CD 2\01.flac"), "A/Album");
        assert_eq!(AlbumFolderPicker::folder_of(r"CD1\01.flac"), "CD1");
        assert_eq!(AlbumFolderPicker::folder_of("01.flac"), "");
    }
}
