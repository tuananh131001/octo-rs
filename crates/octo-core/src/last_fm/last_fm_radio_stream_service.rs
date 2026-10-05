//! The pure half of `Services/LastFm/LastFmRadioStreamService.cs`: the flow picker that orders a
//! continuous stream by how smoothly one track follows another, and the tags it judges kinship
//! by. The streaming itself is `octo::services::last_fm::last_fm_radio_stream_service`.

use std::collections::HashSet;

use crate::common::dotnet;
use crate::last_fm::last_fm_radio_audio_transcoder::RadioAudioProfile;
use crate::last_fm::last_fm_radio_recommendation_service::canonical_tag;
use crate::metadata::GenreNormalizer;
use crate::settings::DiscoveryStationSettings;

/// How many complete MP3 segments a published session starts with.
pub const READY_POOL_SIZE: usize = 3;

/// How many upcoming snapshot tracks the flow picker may choose between.
pub const FLOW_WINDOW: usize = 4;

/// Weight of kinship (tags, genre) against sound in the flow score. At 1.5 a track from another
/// genre costs more than an octave of brightness.
pub const KINSHIP_WEIGHT: f64 = 1.5;

/// Last.fm tags with the ones that say nothing about kinship removed: years, the artist's own
/// name, and the listener-bookkeeping tags the recommender also ignores. Two tracks by the same
/// artist already share their sound; a shared "2018" would only inflate the overlap.
pub fn kinship_tags<S: AsRef<str>>(tags: &[S], artist: &str) -> Vec<String> {
    let artist_tag = DiscoveryStationSettings::normalize_tag(artist);
    let mut seen = HashSet::new();
    tags.iter()
        .map(|tag| canonical_tag(tag.as_ref()))
        .filter(|tag| {
            !tag.is_empty()
                && !dotnet::eq_ignore_case(tag, &artist_tag)
                // One year rule, shared with the genre normaliser. Two hand-rolled copies is how
                // one of them quietly stops dropping "2020s" when someone fixes the other.
                && !GenreNormalizer::is_year_like(tag)
        })
        .filter(|tag| seen.insert(dotnet::ordinal_ignore_case_key(tag)))
        .take(8)
        .collect()
}

/// How far apart two tracks are as neighbours in a stream, as one score with two halves.
/// Sound: brightness as octaves between spectral centroids, dynamics as loudness range, texture
/// as spectral flatness (loudness itself is not a term because every track has been brought to
/// the same level). Kinship: one minus the overlap of their Last.fm tags, the catalogue genre
/// when tags are missing, and a neutral middle when nothing is known so an unmeasured track is
/// neither favored nor punished.
pub fn flow_distance(current: &RadioAudioProfile, next: &RadioAudioProfile) -> f64 {
    let brightness = (next.spectral_centroid_hz.max(20.0) / current.spectral_centroid_hz.max(20.0))
        .log2()
        .abs();
    let dynamics = (next.loudness_range_lu - current.loudness_range_lu).abs() / 5.0;
    let texture = (next.spectral_flatness - current.spectral_flatness).abs() * 5.0;
    brightness + dynamics + texture + KINSHIP_WEIGHT * estrangement(current, next)
}

/// 0 for the same tags, 1 for none in common, by genre when tags are missing.
pub fn estrangement(current: &RadioAudioProfile, next: &RadioAudioProfile) -> f64 {
    if let (Some(mine), Some(theirs)) = (&current.tags, &next.tags)
        && !mine.is_empty()
        && !theirs.is_empty()
    {
        let mine_keys: Vec<String> = distinct_keys(mine);
        let theirs_keys: HashSet<String> = distinct_keys(theirs).into_iter().collect();
        let shared = mine_keys.iter().filter(|key| theirs_keys.contains(*key)).count();
        let union = mine_keys.len() + theirs_keys.iter().filter(|key| !mine_keys.contains(key)).count();
        return if union == 0 {
            0.35
        } else {
            1.0 - shared as f64 / union as f64
        };
    }
    if let (Some(mine), Some(theirs)) = (&current.genre, &next.genre)
        && !dotnet::is_blank(mine)
        && !dotnet::is_blank(theirs)
    {
        return if dotnet::eq_ignore_case(mine, theirs) {
            0.0
        } else {
            0.7
        };
    }
    0.35
}

fn distinct_keys(tags: &[String]) -> Vec<String> {
    let mut keys = Vec::new();
    for tag in tags {
        let key = dotnet::ordinal_ignore_case_key(tag);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Among the next few snapshot tracks, the one that follows the current track most smoothly.
/// Only tracks that are already cached and measured can be compared; when none is, or there is
/// nothing to compare against, the answer is snapshot order, which is what the stream did before
/// profiles existed.
pub fn choose_by_flow(current: Option<&RadioAudioProfile>, window: &[Option<RadioAudioProfile>]) -> usize {
    let Some(current) = current else { return 0 };
    if window.is_empty() {
        return 0;
    }
    let mut best = 0;
    let mut best_distance = f64::INFINITY;
    for (offset, profile) in window.iter().enumerate() {
        let Some(profile) = profile else { continue };
        let distance = flow_distance(current, profile);
        if distance < best_distance {
            best_distance = distance;
            best = offset;
        }
    }
    if best_distance == f64::INFINITY { 0 } else { best }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(range: f64, centroid: f64, flatness: f64, rolloff: f64) -> RadioAudioProfile {
        RadioAudioProfile::new(-16.0, range, -1.0, 0.0, centroid, flatness, rolloff)
    }

    fn tagged(
        mut profile: RadioAudioProfile,
        genre: Option<&str>,
        tags: Option<&[&str]>,
    ) -> RadioAudioProfile {
        profile.genre = genre.map(str::to_string);
        profile.tags = tags.map(|tags| tags.iter().map(|t| t.to_string()).collect());
        profile
    }

    // LastFmRadioCoreTests.Flow_PrefersTheClosestMeasuredTrackAndFallsBackToSnapshotOrder
    #[test]
    fn flow_prefers_the_closest_measured_track_and_falls_back_to_snapshot_order() {
        let current = profile(6.0, 2000.0, 0.10, 6000.0);
        let bright = profile(6.0, 6500.0, 0.12, 12000.0);
        let close = profile(7.0, 2200.0, 0.11, 6500.0);
        let noisy = profile(4.0, 2100.0, 0.60, 7000.0);

        assert_eq!(
            choose_by_flow(
                Some(&current),
                &[Some(bright.clone()), Some(close.clone()), Some(noisy)]
            ),
            1
        );
        assert_eq!(
            choose_by_flow(Some(&current), &[None, None, Some(close.clone())]),
            2
        );
        assert_eq!(choose_by_flow(Some(&current), &[None, None, None]), 0);
        assert_eq!(
            choose_by_flow(None, &[Some(bright.clone()), Some(close.clone())]),
            0
        );
        assert!(flow_distance(&current, &close) < flow_distance(&current, &bright));
    }

    // LastFmRadioCoreTests.KinshipTags_DropYearsTheArtistAndBookkeeping
    #[test]
    fn kinship_tags_drop_years_the_artist_and_bookkeeping() {
        let kept = kinship_tags(
            &[
                "2018",
                "Xavier Wulf",
                "Hip Hop",
                "seen live",
                "Cloud Rap",
                "00s",
                "hip hop",
                "phonk",
            ],
            "Xavier Wulf",
        );
        assert_eq!(kept, ["hip-hop", "cloud rap", "phonk"]);
    }

    // LastFmRadioCoreTests.Flow_WeighsKinshipBesideSound
    #[test]
    fn flow_weighs_kinship_beside_sound() {
        // The current track is trap. One candidate sounds almost identical but is indie rock;
        // the other is an octave brighter but shares every tag. Kinship outweighs an octave,
        // so the brighter trap track follows.
        let current = tagged(
            profile(6.0, 2000.0, 0.10, 6000.0),
            Some("Hip-Hop"),
            Some(&["hip-hop", "trap"]),
        );
        let stranger_that_sounds_close = tagged(
            profile(6.0, 2100.0, 0.11, 6200.0),
            Some("Rock"),
            Some(&["indie rock", "rock"]),
        );
        let kin_that_sounds_brighter = tagged(
            profile(6.0, 4000.0, 0.10, 9000.0),
            Some("Hip-Hop"),
            Some(&["hip-hop", "trap"]),
        );
        assert_eq!(
            choose_by_flow(
                Some(&current),
                &[Some(stranger_that_sounds_close), Some(kin_that_sounds_brighter)]
            ),
            1
        );

        // Half the tags in common sits between the two; genre alone decides when tags are
        // missing; nothing known is a neutral middle rather than a verdict.
        let half_kin = tagged(current.clone(), Some("Hip-Hop"), Some(&["trap", "cloud rap"]));
        let half = estrangement(&current, &half_kin);
        assert!((0.5..=0.75).contains(&half), "{half}");
        assert_eq!(
            estrangement(&current, &tagged(current.clone(), Some("Hip-Hop"), None)),
            0.0
        );
        assert_eq!(
            estrangement(
                &tagged(current.clone(), Some("Hip-Hop"), None),
                &tagged(current.clone(), Some("Jazz"), None)
            ),
            0.7
        );
        assert_eq!(
            estrangement(
                &tagged(current.clone(), None, None),
                &tagged(current.clone(), None, None)
            ),
            0.35
        );
    }

    /// GenreNormalizerTests.IsYearLike_MatchesWhatTheRadioKinshipFilterAlsoDrops, the radio
    /// half: kinship drops exactly the tags the genre normaliser calls a year.
    #[test]
    fn is_year_like_matches_what_the_radio_kinship_filter_also_drops() {
        for (tag, expected) in [
            ("1998", true),
            ("2026", true),
            ("80", true),
            ("90s", true),
            ("1990s", true),
            ("2020s", true),
            ("nu metal", false),
            ("4ad", false),
        ] {
            assert_eq!(GenreNormalizer::is_year_like(tag), expected, "{tag}");
            let kinship = kinship_tags(&[tag], "Some Artist");
            assert_eq!(
                expected,
                !kinship.iter().any(|kept| dotnet::eq_ignore_case(kept, tag)),
                "{tag}"
            );
        }
    }
}
