//! The pure half of `Services/LastFm/LastFmRadioRecommendationService.cs`: how candidates are
//! weighed, drawn and shaped into a station, and how a listener's plays become seeds, artists
//! and tags. The service that asks Last.fm is
//! `octo::services::last_fm::last_fm_radio_recommendation_service`.
//!
//! Which candidates make the cut is a weighted draw rather than a fixed top-N, so two refreshes
//! of the same profile give two different stations; the previous snapshot is demoted so a
//! refresh rotates the station instead of restating it.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};

use crate::common::dotnet;
use crate::common::dotnet_random::DotnetRandom;
use crate::last_fm::SimilarTrack;
use crate::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use crate::last_fm::last_fm_radio_state_store::station_id;
use crate::metadata::genre_normalizer::to_title_case;
use crate::models::radio::{LastFmRadioPlay, LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack};
use crate::settings::{DiscoveryStationSettings, LastFmSettings};

/// Share of its weight a track keeps when it was already in this station's previous snapshot.
/// Low enough that a refresh is mostly new, high enough that a strong match can still come back.
pub const PREVIOUS_SNAPSHOT_WEIGHT: f64 = 0.35;

/// How fast a provider's ranking fades. Rank r keeps 1 / (1 + r / depth) of its weight, so the
/// top of a tag's or an artist's list still leads every draw and the tail is where refreshes
/// differ. 20 is the middle of ListenBrainz's easy/medium/hard popularity windows: reachable,
/// not bottom of the barrel.
pub const RANK_HALF_DEPTH: f64 = 20.0;

/// Top tracks of an artist similar to the seed, relative to the seed's own.
pub const NEIGHBOUR_ARTIST_AFFINITY: f64 = 0.6;

/// Each older seed contributes this much of the previous one; hearts count half again.
pub const SEED_RECENCY_DECAY: f64 = 0.85;
pub const HEARTED_SEED_AFFINITY: f64 = 1.5;

pub const FAMILIAR_SOURCE: &str = "history";

/// How much a play counts as a listening signal, by where it came from. A track the listener
/// chose and scrobbled is the real signal. A track the radio played to the end says less: they
/// did not pick it, they only did not switch it off. Left at full weight, a station slowly
/// trains itself on its own output. A play recorded at bootstrap from a random library track
/// says less still.
pub const RADIO_PLAY_WEIGHT: f64 = 0.4;
pub const RANDOM_BOOTSTRAP_WEIGHT: f64 = 0.5;

/// The source of a build's draw: `System.Random`'s `NextDouble`.
pub trait RadioRandom: Send {
    fn next_double(&mut self) -> f64;
}

impl RadioRandom for DotnetRandom {
    fn next_double(&mut self) -> f64 {
        DotnetRandom::next_double(self)
    }
}

/// `Func<Random>`: a fresh draw source per build. Tests replace it with a seeded one so a build
/// repeats exactly.
pub type Randomizer = Arc<dyn Fn() -> Box<dyn RadioRandom> + Send + Sync>;

/// `() => new Random()`: an unseeded draw, different every build.
pub fn default_randomizer() -> Randomizer {
    Arc::new(|| Box::new(DotnetRandom::new(rand::random::<i32>())) as Box<dyn RadioRandom>)
}

/// `() => new Random(seed)`.
pub fn seeded_randomizer(seed: i32) -> Randomizer {
    Arc::new(move || Box::new(DotnetRandom::new(seed)) as Box<dyn RadioRandom>)
}

/// A candidate with its final draw weight (match x rank decay x seed affinity) and the list it
/// came from: a seed track, a tag, an artist, or the listener's history.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub track: SimilarTrack,
    pub weight: f64,
    pub source: String,
}

/// How much a play counts, by where it came from (see [`RADIO_PLAY_WEIGHT`]).
pub fn source_weight(play: &LastFmRadioPlay) -> f64 {
    match play.source.as_str() {
        "internet-radio" => RADIO_PLAY_WEIGHT,
        "bootstrap-random" => RANDOM_BOOTSTRAP_WEIGHT,
        _ => 1.0,
    }
}

const DENIED_TAGS: [&str; 29] = [
    "seen live",
    "favorites",
    "favorites",
    "owned",
    "spotify",
    "albums i own",
    "under 2000 listeners",
    "awesome",
    "love",
    "best",
    // Sentiment and superlatives say how a listener felt, not what the music is.
    "favorite song",
    "favorite song",
    "favorite songs",
    "favorite songs",
    "my love",
    "love at first listen",
    "beautiful",
    "epic",
    "legendary",
    "classic",
    "amazing",
    "perfect",
    "masterpiece",
    "good",
    "great",
    "catchy",
    "fun",
    "chill",
    "cool",
];

const TAG_ALIASES: [(&str, &str); 6] = [
    ("electronica", "electronic"),
    ("hip hop", "hip-hop"),
    ("hiphop", "hip-hop"),
    ("rnb", "r&b"),
    ("rhythm and blues", "r&b"),
    ("alt rock", "alternative rock"),
];

/// One spelling per tag, shared by seeding and by kinship: lower case, single spaces, the alias
/// table applied ("hip hop" and "hiphop" are "hip-hop"), and empty for tags that describe the
/// listener rather than the music ("seen live", "owned", "favorites").
pub fn canonical_tag(value: &str) -> String {
    let mut tag = DiscoveryStationSettings::normalize_tag(value);
    if let Some((_, alias)) = TAG_ALIASES
        .iter()
        .find(|(from, _)| dotnet::eq_ignore_case(from, &tag))
    {
        tag = (*alias).to_string();
    }
    if DENIED_TAGS
        .iter()
        .any(|denied| dotnet::eq_ignore_case(denied, &tag))
    {
        String::new()
    } else {
        tag
    }
}

/// How many tracks one source list may hold: an even share and a half, never fewer than three.
/// Six seeds feeding a 50-track mix each get at most 13; a station built from one tag is
/// unconstrained by it.
pub fn source_cap(target: i32, sources: usize) -> i32 {
    let share = 1.5 * f64::from(target) / sources.max(1) as f64;
    3.max(share.ceil() as i32)
}

/// The seed ranking: provenance, hearts, and a 45-day recency decay.
pub fn seed_score(play: &LastFmRadioPlay, now: DateTime<Utc>) -> f64 {
    let days = (now - play.played_at_utc).num_microseconds().map_or_else(
        || (now - play.played_at_utc).num_milliseconds() as f64 / 86_400_000.0,
        |micros| micros as f64 / 86_400_000_000.0,
    );
    source_weight(play) * (if play.hearted { 2.0 } else { 1.0 }) * (-days / 45.0).exp()
}

/// How many tracks one artist may hold in a station. An artist station is about its artist, so a
/// quarter of it may be theirs; everywhere else an artist is a guest.
pub fn artist_cap(settings: &LastFmSettings, kind: LastFmRadioStationKind) -> i32 {
    if kind == LastFmRadioStationKind::Artist {
        3.max(settings.effective_radio_track_count() / 4)
    } else {
        2.max(settings.effective_radio_track_count() / 10)
    }
}

/// A play as a candidate: hearted counts double, provenance scales it, and its place in the
/// recency order decays the same way a provider rank does.
pub fn to_candidate(play: &LastFmRadioPlay, recency_rank: usize) -> Candidate {
    weigh(
        SimilarTrack::new(
            play.artist.clone(),
            play.title.clone(),
            if play.hearted { 2.0 } else { 1.0 },
        )
        .with_duration(play.duration),
        recency_rank,
        FAMILIAR_SOURCE,
        source_weight(play),
    )
}

fn rank_decay(rank: usize) -> f64 {
    1.0 / (1.0 + rank as f64 / RANK_HALF_DEPTH)
}

pub fn weigh(track: SimilarTrack, rank: usize, source: &str, affinity: f64) -> Candidate {
    let weight = track.r#match.max(0.05) * rank_decay(rank) * affinity;
    Candidate {
        track,
        weight,
        source: source.to_string(),
    }
}

/// Weights a provider's list in the order it came, which is the provider's ranking.
pub fn ranked(tracks: Vec<SimilarTrack>, source: &str, affinity: f64) -> Vec<Candidate> {
    tracks
        .into_iter()
        .enumerate()
        .map(|(rank, track)| weigh(track, rank, source, affinity))
        .collect()
}

/// A set of strings compared with `StringComparer.OrdinalIgnoreCase`.
#[derive(Debug, Clone, Default)]
pub struct IgnoreCaseKeys(HashSet<String>);

impl IgnoreCaseKeys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, value: &str) -> bool {
        self.0.insert(dotnet::ordinal_ignore_case_key(value))
    }

    pub fn contains(&self, value: &str) -> bool {
        self.0.contains(&dotnet::ordinal_ignore_case_key(value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<S: AsRef<str>> FromIterator<S> for IgnoreCaseKeys {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut keys = IgnoreCaseKeys::new();
        for value in iter {
            keys.insert(value.as_ref());
        }
        keys
    }
}

/// Selects the station's tracks from its weighted candidates. One weighted draw orders the pool
/// (see [`weighted_order`]), then the walk applies the spacing rules: nothing unavailable,
/// nothing just played when asked, never the same artist twice in a row, no artist past its cap
/// for this station kind, no source list (one seed's neighbours, one tag, one artist) past its
/// share, and when a familiar quota is set, that many tracks from the listener's own history and
/// the rest from discovery. The caps are the marginal-relevance idea applied greedily, with
/// artist and source repetition as the redundancy.
pub fn shape(
    candidates: Vec<Candidate>,
    plays: &[LastFmRadioPlay],
    settings: &LastFmSettings,
    unavailable: &IgnoreCaseKeys,
    random: &mut dyn RadioRandom,
    previous: &IgnoreCaseKeys,
    artist_cap: i32,
    exclude_recent: bool,
    familiar_quota: Option<i32>,
) -> Vec<LastFmRadioTrack> {
    let target = settings.effective_radio_track_count();
    let recent: IgnoreCaseKeys = plays
        .iter()
        .take(30)
        .map(|play| seed_normalizer::track_key(&play.artist, &play.title))
        .collect();
    let mut last_artist = String::new();
    let mut per_artist: HashMap<String, i32> = HashMap::new();
    let mut per_source: HashMap<String, i32> = HashMap::new();
    let mut familiar_taken = 0;
    let mut output: Vec<LastFmRadioTrack> = Vec::new();

    // GroupBy the track key (ordinal), keeping each group's heaviest, the first of equals.
    let mut groups: IndexMap<String, Candidate> = IndexMap::new();
    for item in candidates
        .into_iter()
        .filter(|item| !item.track.artist.is_empty() && !item.track.title.is_empty())
    {
        let key = seed_normalizer::track_key(&item.track.artist, &item.track.title);
        match groups.get_mut(&key) {
            Some(best) => {
                if item.weight > best.weight {
                    *best = item;
                }
            }
            None => {
                groups.insert(key, item);
            }
        }
    }
    let distinct: Vec<Candidate> = groups.into_values().collect();
    let source_cap = source_cap(
        target,
        distinct
            .iter()
            .map(|item| item.source.as_str())
            .collect::<IgnoreCaseKeys>()
            .len(),
    );
    for candidate in weighted_order(distinct, random, previous) {
        let track = &candidate.track;
        let key = seed_normalizer::track_key(&track.artist, &track.title);
        if unavailable.contains(&key) {
            continue;
        }
        if exclude_recent && recent.contains(&key) {
            continue;
        }
        if dotnet::eq_ignore_case(&last_artist, &track.artist) {
            continue;
        }
        let artist_key = dotnet::ordinal_ignore_case_key(&track.artist);
        let source_key = dotnet::ordinal_ignore_case_key(&candidate.source);
        if per_artist.get(&artist_key).copied().unwrap_or(0) >= artist_cap {
            continue;
        }
        if per_source.get(&source_key).copied().unwrap_or(0) >= source_cap {
            continue;
        }
        let is_familiar = candidate.source == FAMILIAR_SOURCE;
        if let Some(quota) = familiar_quota {
            if is_familiar && familiar_taken >= quota {
                continue;
            }
            if !is_familiar && output.len() as i32 - familiar_taken >= target - quota {
                continue;
            }
        }
        output.push(LastFmRadioTrack {
            artist: seed_normalizer::artist(&track.artist),
            title: seed_normalizer::title(&track.title),
            duration: track.duration,
            score: track.r#match,
            source: "lastfm".to_string(),
            ..Default::default()
        });
        last_artist = track.artist.clone();
        *per_artist.entry(artist_key).or_insert(0) += 1;
        *per_source.entry(source_key).or_insert(0) += 1;
        if is_familiar {
            familiar_taken += 1;
        }
        if output.len() as i32 >= target {
            break;
        }
    }
    output
}

/// One weighted draw over the candidates (Efraimidis-Spirakis: each candidate draws
/// u^(1/weight) and the pool is sorted by that), so a track's chance of landing near the front
/// is proportional to its weight and every build draws differently. Tracks that were in this
/// station's previous snapshot keep [`PREVIOUS_SNAPSHOT_WEIGHT`] of their weight. Ties fall back
/// to the stable hash so equal draws are not order-of-arrival.
pub fn weighted_order(
    candidates: Vec<Candidate>,
    random: &mut dyn RadioRandom,
    previous: &IgnoreCaseKeys,
) -> Vec<Candidate> {
    let mut drawn: Vec<(Candidate, f64, String)> = candidates
        .into_iter()
        .map(|item| {
            let draw = random.next_double().powf(1.0 / weight(&item, previous));
            let order = stable_order(&item.track.artist, &item.track.title);
            (item, draw, order)
        })
        .collect();
    drawn.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
    });
    drawn.into_iter().map(|(item, _, _)| item).collect()
}

fn weight(item: &Candidate, previous: &IgnoreCaseKeys) -> f64 {
    let weight = item.weight.max(0.01);
    if previous.contains(&seed_normalizer::track_key(&item.track.artist, &item.track.title)) {
        weight * PREVIOUS_SNAPSHOT_WEIGHT
    } else {
        weight
    }
}

/// A station keeps only the tracks no earlier station has, unless that would leave it with
/// fewer than ten (or all it had).
pub fn suppress_station_overlap(stations: &mut [LastFmRadioStation]) {
    let mut used = IgnoreCaseKeys::new();
    for station in stations.iter_mut() {
        let unique: Vec<LastFmRadioTrack> = station
            .tracks
            .iter()
            .filter(|track| !used.contains(&seed_normalizer::track_key(&track.artist, &track.title)))
            .cloned()
            .collect();
        if unique.len() >= 10.min(station.tracks.len()) {
            station.tracks = unique;
        }
        for track in &station.tracks {
            used.insert(&seed_normalizer::track_key(&track.artist, &track.title));
        }
    }
}

/// `Comparer<string>.Default` under the invariant culture, as `ThenBy(pair => pair.Key)` sorted:
/// case is compared after the letters, lower case first.
pub fn invariant_culture_compare(a: &str, b: &str) -> Ordering {
    dotnet::to_lower_invariant(a)
        .cmp(&dotnet::to_lower_invariant(b))
        .then_with(|| {
            for (x, y) in a.chars().zip(b.chars()) {
                if x != y {
                    // Lower case sorts before upper case at the tertiary level.
                    return match (dotnet::is_lower(x), dotnet::is_lower(y)) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => x.cmp(&y),
                    };
                }
            }
            a.len().cmp(&b.len())
        })
}

/// Each artist's three most recent plays, by seed score: the strongest artists first, then by
/// name. Keyed by the primary artist as the first play spelled it.
pub fn score_artists(plays: &[LastFmRadioPlay], now: DateTime<Utc>) -> Vec<(String, f64)> {
    // GroupBy (OrdinalIgnoreCase): the first spelling names the group, in first-seen order.
    let mut groups: IndexMap<String, (String, Vec<&LastFmRadioPlay>)> = IndexMap::new();
    for play in plays {
        let name = seed_normalizer::artist(&play.artist);
        groups
            .entry(dotnet::ordinal_ignore_case_key(&name))
            .or_insert_with(|| (name, Vec::new()))
            .1
            .push(play);
    }
    let mut scores: Vec<(String, f64)> = groups
        .into_values()
        .map(|(name, plays)| {
            let score = plays.iter().take(3).map(|play| seed_score(play, now)).sum();
            (name, score)
        })
        .collect();
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| invariant_culture_compare(&a.0, &b.0))
    });
    scores
}

/// Tag scores, keyed ignoring case, in the order the tags were first scored.
#[derive(Debug, Clone, Default)]
pub struct TagScores {
    scores: IndexMap<String, (String, f64)>,
}

impl TagScores {
    /// `AddTag`: the canonical spelling, skipped when it is empty.
    pub fn add(&mut self, value: &str, score: f64) {
        let tag = canonical_tag(value);
        if tag.is_empty() {
            return;
        }
        self.scores
            .entry(dotnet::ordinal_ignore_case_key(&tag))
            .or_insert_with(|| (tag, 0.0))
            .1 += score;
    }

    /// The tags with their scores, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, f64)> {
        self.scores.values().map(|(tag, score)| (tag.as_str(), *score))
    }

    /// `OrderByDescending(Value).ThenBy(Key).Take(n)`: the discovery mix's tags.
    pub fn top_by_score_then_name(&self, n: usize) -> Vec<String> {
        let mut tags: Vec<(&str, f64)> = self.iter().collect();
        tags.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| invariant_culture_compare(a.0, b.0))
        });
        tags.into_iter().take(n).map(|(tag, _)| tag.to_string()).collect()
    }

    /// `OrderByDescending(Value).Take(n)`: the genre stations' tags, equal scores in the order
    /// they were first scored.
    pub fn top_by_score(&self, n: usize) -> Vec<String> {
        let mut tags: Vec<(&str, f64)> = self.iter().collect();
        tags.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        tags.into_iter().take(n).map(|(tag, _)| tag.to_string()).collect()
    }
}

/// The genres of the listener's plays, each worth two plays scaled by provenance.
pub fn score_local_tags(plays: &[LastFmRadioPlay]) -> TagScores {
    let mut tags = TagScores::default();
    for play in plays {
        if let Some(genre) = play.genre.as_deref().filter(|genre| !dotnet::is_blank(genre)) {
            tags.add(genre, 2.0 * source_weight(play));
        }
    }
    tags
}

/// A new station snapshot, valid for twelve hours until the build sets its own validity.
pub fn create<S: AsRef<str>>(
    username: &str,
    key: &str,
    name: &str,
    kind: LastFmRadioStationKind,
    personalized: bool,
    seeds: &[S],
    tracks: Vec<LastFmRadioTrack>,
    definition_version: i32,
    now: DateTime<Utc>,
) -> LastFmRadioStation {
    let mut distinct = IgnoreCaseKeys::new();
    LastFmRadioStation {
        id: station_id(username, key),
        key: key.to_string(),
        name: name.to_string(),
        owner: username.to_string(),
        kind,
        personalized,
        definition_version,
        created_utc: now,
        changed_utc: now,
        valid_until_utc: now + TimeDelta::hours(12),
        seeds: seeds
            .iter()
            .map(AsRef::as_ref)
            .filter(|seed| distinct.insert(seed))
            .map(str::to_string)
            .collect(),
        tracks,
    }
}

/// A pinned station's version: the first four bytes of SHA-256 over its id, name and tags, so
/// editing a definition changes it.
pub fn definition_version(settings: &DiscoveryStationSettings) -> i32 {
    let text = format!("{}|{}|{}", settings.id, settings.name, settings.tags.join("|"));
    let hash = Sha256::digest(text.as_bytes());
    i32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// The first six bytes of SHA-256 over the lower-cased value, as lower-case hex: the station
/// key of an artist or genre station.
pub fn key(value: &str) -> String {
    let hash = Sha256::digest(dotnet::to_lower_invariant(value).as_bytes());
    hex::encode(&hash[..6])
}

/// `TextInfo.ToTitleCase(value.ToLowerInvariant())` in the invariant culture.
pub fn title(value: &str) -> String {
    to_title_case(&dotnet::to_lower_invariant(value))
}

fn stable_order(artist: &str, title: &str) -> String {
    key(&format!("{artist}|{title}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What .NET 9 computed.
    #[test]
    fn keys_titles_and_versions_are_what_dotnet_computed() {
        assert_eq!(key("Björk"), "590a34408cfe");
        assert_eq!(key("Fresh"), "d098ab5e44b9");
        assert_eq!(key("A|T"), "f115f662b5f4");
        assert_eq!(title("hip-hop"), "Hip-Hop");
        assert_eq!(title("r&b"), "R&B");
        assert_eq!(title("alternative rock"), "Alternative Rock");
        let rock = DiscoveryStationSettings {
            id: "rock".into(),
            name: "Rock".into(),
            tags: vec!["rock".into()],
            ..Default::default()
        };
        assert_eq!(definition_version(&rock), 1653469986);
        let fusion = DiscoveryStationSettings {
            id: "fusion".into(),
            name: "Fusion".into(),
            tags: vec!["rock".into(), "idm".into()],
            ..Default::default()
        };
        assert_eq!(definition_version(&fusion), -596081354);
    }

    #[test]
    fn canonical_tags_apply_aliases_and_the_deny_list() {
        assert_eq!(canonical_tag(" Hip  Hop "), "hip-hop");
        assert_eq!(canonical_tag("Electronica"), "electronic");
        assert_eq!(canonical_tag("Seen Live"), "");
        assert_eq!(canonical_tag("RnB"), "r&b");
        assert_eq!(canonical_tag("Shoegaze"), "shoegaze");
    }

    #[test]
    fn caps_follow_the_station_size() {
        assert_eq!(source_cap(50, 6), 13);
        assert_eq!(source_cap(10, 1), 15);
        assert_eq!(source_cap(10, 20), 3);
        assert_eq!(source_cap(10, 0), 15);
        let settings = LastFmSettings {
            radio_track_count: 50,
            ..Default::default()
        };
        assert_eq!(artist_cap(&settings, LastFmRadioStationKind::Artist), 12);
        assert_eq!(artist_cap(&settings, LastFmRadioStationKind::YourMix), 5);
    }

    #[test]
    fn plays_from_the_radio_count_less() {
        let play = |source: &str| LastFmRadioPlay {
            source: source.into(),
            ..Default::default()
        };
        assert_eq!(source_weight(&play("internet-radio")), 0.4);
        assert_eq!(source_weight(&play("bootstrap-random")), 0.5);
        assert_eq!(source_weight(&play("scrobble")), 1.0);
    }

    /// A flat draw orders by weight; the previous snapshot falls behind fresh candidates.
    #[test]
    fn a_flat_draw_orders_by_weight_and_demotes_the_previous_snapshot() {
        struct Flat;
        impl RadioRandom for Flat {
            fn next_double(&mut self) -> f64 {
                0.5
            }
        }
        let candidates = ranked(
            (0..4)
                .map(|i| SimilarTrack::new(format!("A{i}"), format!("T{i}"), 1.0))
                .collect(),
            "tag:x",
            1.0,
        );
        let order: Vec<String> = weighted_order(candidates.clone(), &mut Flat, &IgnoreCaseKeys::new())
            .into_iter()
            .map(|c| c.track.title)
            .collect();
        assert_eq!(order, ["T0", "T1", "T2", "T3"]);
        let previous: IgnoreCaseKeys = [seed_normalizer::track_key("A0", "T0")].into_iter().collect();
        let order: Vec<String> = weighted_order(candidates, &mut Flat, &previous)
            .into_iter()
            .map(|c| c.track.title)
            .collect();
        assert_eq!(order, ["T1", "T2", "T3", "T0"]);
    }

    #[test]
    fn overlapping_stations_keep_only_new_tracks_when_enough_are_left() {
        let track = |artist: &str| LastFmRadioTrack {
            artist: artist.into(),
            title: "T".into(),
            ..Default::default()
        };
        let mut stations = vec![
            LastFmRadioStation {
                tracks: vec![track("A"), track("B")],
                ..Default::default()
            },
            LastFmRadioStation {
                tracks: vec![track("a"), track("C")],
                ..Default::default()
            },
        ];
        suppress_station_overlap(&mut stations);
        // Only one unique track of two: fewer than min(10, 2), so the second keeps both.
        assert_eq!(stations[1].tracks.len(), 2);
        let mut stations = vec![
            LastFmRadioStation {
                tracks: vec![track("A")],
                ..Default::default()
            },
            LastFmRadioStation {
                tracks: (0..12)
                    .map(|i| track(&format!("X{i}")))
                    .chain([track("A")])
                    .collect(),
                ..Default::default()
            },
        ];
        suppress_station_overlap(&mut stations);
        assert_eq!(stations[1].tracks.len(), 12);
    }

    #[test]
    fn invariant_culture_order_puts_case_after_letters() {
        assert_eq!(invariant_culture_compare("a", "B"), Ordering::Less);
        assert_eq!(invariant_culture_compare("b", "A"), Ordering::Greater);
        assert_eq!(invariant_culture_compare("a", "A"), Ordering::Less);
        assert_eq!(invariant_culture_compare("rock", "rock"), Ordering::Equal);
    }
}
