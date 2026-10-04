//! Port of `Services/Tagging/AlbumTagContext.cs`.

use parking_lot::Mutex;

use crate::common::dotnet::eq_ignore_case;
use crate::common::song_identity::SongIdentity;
use crate::models::domain::song::Song;
use crate::tagging::net::ignore_case_key;
use crate::tagging::tag_plan::TagPlan;

/// The release facts a walk settled on, from its first identified track.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettledRelease {
    pub release_id: Option<String>,
    pub release_group_id: Option<String>,
    pub music_brainz_album_title: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub barcode: Option<String>,
    pub release_type: Option<String>,
    pub release_status: Option<String>,
    pub release_country: Option<String>,
    pub year: Option<i32>,
    pub original_date: Option<String>,
    pub release_date: Option<String>,
    pub total_tracks: Option<i32>,
    pub album_artist_ids: Vec<String>,
    pub captured: bool,
}

/// What one album walk has settled, so every track of the walk shares the album-level fields:
/// the release the first track matched, its facts, and each track's measured loudness for the
/// album gain at the end. Two tracks of one album matched to two pressings would otherwise
/// carry two labels and two catalogue numbers, and the library server shows one album's facts
/// from whichever track it reads first.
///
/// Shared by the tracks of a walk, so its state sits behind locks. `L` is the loudness type
/// (`octo_media::audio::Loudness`, which this crate cannot name).
#[derive(Debug)]
pub struct AlbumTagContext<L = ()> {
    catalog_album_id: Option<String>,
    album_title: String,
    album_artist: Option<String>,
    settled: Mutex<SettledRelease>,
    /// Each placed track's path and loudness, keyed ignoring case, in the order first placed.
    loudness: Mutex<indexmap::IndexMap<String, (String, Option<L>)>>,
}

impl<L: Clone> AlbumTagContext<L> {
    pub fn new(catalog_album_id: Option<&str>, album_title: &str, album_artist: Option<&str>) -> Self {
        Self {
            catalog_album_id: catalog_album_id.map(str::to_string),
            album_title: album_title.to_string(),
            album_artist: album_artist.map(str::to_string),
            settled: Mutex::new(SettledRelease::default()),
            loudness: Mutex::new(indexmap::IndexMap::new()),
        }
    }

    pub fn catalog_album_id(&self) -> Option<&str> {
        self.catalog_album_id.as_deref()
    }

    pub fn album_title(&self) -> &str {
        &self.album_title
    }

    pub fn album_artist(&self) -> Option<&str> {
        self.album_artist.as_deref()
    }

    /// A copy of what the walk has settled so far.
    pub fn settled(&self) -> SettledRelease {
        self.settled.lock().clone()
    }

    pub fn captured(&self) -> bool {
        self.settled.lock().captured
    }

    pub fn release_id(&self) -> Option<String> {
        self.settled.lock().release_id.clone()
    }

    /// `Loudness[path] = value`: the C# dictionary ignored the path's case, and a path already
    /// there keeps the spelling it was first placed with.
    pub fn set_loudness(&self, path: &str, value: Option<L>) {
        let mut loudness = self.loudness.lock();
        match loudness.get_mut(&ignore_case_key(path)) {
            Some(entry) => entry.1 = value,
            None => {
                loudness.insert(ignore_case_key(path), (path.to_string(), value));
            }
        }
    }

    /// Every placed track's path and loudness, in the order they were first placed.
    pub fn loudness(&self) -> Vec<(String, Option<L>)> {
        self.loudness.lock().values().cloned().collect()
    }

    /// Remember the first track's release facts, once a plan set them from a release.
    pub fn capture(&self, plan: &TagPlan, song: &Song) {
        let mut settled = self.settled.lock();
        let Some(chosen) = plan.chosen.as_ref() else {
            return;
        };
        if settled.captured || !plan.album_from_candidate() {
            return;
        }
        if SongIdentity::key(&song.album) != SongIdentity::key(&self.album_title) {
            return;
        }
        *settled = SettledRelease {
            release_id: song
                .music_brainz_release_id
                .clone()
                .or_else(|| chosen.candidate.release_id.clone()),
            release_group_id: song
                .music_brainz_release_group_id
                .clone()
                .or_else(|| chosen.candidate.release_group_id.clone()),
            music_brainz_album_title: song.music_brainz_album_title.clone(),
            label: song.label.clone(),
            catalog_number: song.catalog_number.clone(),
            barcode: song.barcode.clone(),
            release_type: song.release_type.clone(),
            release_status: song.release_status.clone(),
            release_country: song.release_country.clone(),
            year: song.year,
            original_date: song.original_date.clone(),
            release_date: song.release_date.clone(),
            total_tracks: song.total_tracks,
            album_artist_ids: song.music_brainz_album_artist_ids.clone(),
            captured: true,
        };
    }

    /// Steer a later track onto the walk's release: when its best candidate is another pressing
    /// of the same group, the candidate on the settled release takes its place.
    pub fn prefer_settled_release(&self, plan: &mut TagPlan) -> bool {
        let settled = self.settled.lock().clone();
        let (true, Some(release_id)) = (settled.captured, settled.release_id.as_deref()) else {
            return false;
        };
        let Some(chosen) = plan.chosen.as_ref() else {
            return false;
        };
        let same = |a: Option<&str>, b: Option<&str>| match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => eq_ignore_case(a, b),
            _ => false,
        };
        if same(chosen.candidate.release_id.as_deref(), Some(release_id)) {
            return false;
        }
        if !same(
            chosen.candidate.release_group_id.as_deref(),
            settled.release_group_id.as_deref(),
        ) {
            return false;
        }
        let Some(found) = plan
            .ranked
            .iter()
            .find(|s| same(s.candidate.release_id.as_deref(), Some(release_id)))
            .cloned()
        else {
            return false;
        };
        let note = format!(
            "the walk's release was kept over another pressing of the same album ({})",
            chosen.candidate.release_date.as_deref().unwrap_or("?")
        );
        plan.chosen = Some(found);
        plan.notes.push(note);
        true
    }

    /// Copy the settled album-level facts onto a track of the same album.
    pub fn pin(&self, song: &mut Song) {
        let settled = self.settled.lock();
        if !settled.captured || SongIdentity::key(&song.album) != SongIdentity::key(&self.album_title) {
            return;
        }
        song.music_brainz_release_id = settled.release_id.clone();
        song.music_brainz_release_group_id = settled.release_group_id.clone();
        song.music_brainz_album_title = settled.music_brainz_album_title.clone();
        song.label = settled.label.clone();
        song.catalog_number = settled.catalog_number.clone();
        song.barcode = settled.barcode.clone();
        song.release_type = settled.release_type.clone();
        song.release_status = settled.release_status.clone();
        song.release_country = settled.release_country.clone();
        if settled.year.is_some() {
            song.year = settled.year;
        }
        song.original_date = settled.original_date.clone();
        song.release_date = settled.release_date.clone();
        if settled.total_tracks.is_some() {
            song.total_tracks = settled.total_tracks;
        }
        song.music_brainz_album_artist_ids = settled.album_artist_ids.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tagging::tag_evidence::{ReleaseCandidate, TagSource};
    use crate::tagging::tag_plan::{ScoredCandidate, TagConfidence};

    fn scored(release: &str, group: &str, date: &str) -> ScoredCandidate {
        ScoredCandidate::new(
            ReleaseCandidate {
                release_id: Some(release.into()),
                release_group_id: Some(group.into()),
                release_title: Some("Mezzanine".into()),
                release_date: Some(date.into()),
                ..ReleaseCandidate::new(TagSource::Fingerprint, "Teardrop", "Massive Attack")
            },
            0.0,
            Vec::new(),
        )
    }

    fn plan(chosen: ScoredCandidate, ranked: Vec<ScoredCandidate>) -> TagPlan {
        TagPlan {
            confidence: TagConfidence::Strong,
            chosen: Some(chosen),
            ranked,
            ..Default::default()
        }
    }

    /// The pure half of `DownloadTaggingTests.TwoTracksOfOneWalk_ShareTheReleaseFacts` (the
    /// download itself is 4-B's): the first track's facts are captured, a second track's other
    /// pressing of the same group is steered onto the settled release, and its song is pinned.
    #[test]
    fn a_walk_captures_steers_and_pins() {
        let context: AlbumTagContext = AlbumTagContext::new(Some("1"), "Mezzanine", Some("Massive Attack"));
        let first = plan(scored("r-1998", "g", "1998-04-20"), vec![]);
        let mut song = Song {
            album: "mezzanine".into(),
            label: Some("Virgin".into()),
            catalog_number: Some("CDV 2851".into()),
            year: Some(1998),
            total_tracks: Some(11),
            ..Default::default()
        };
        context.capture(&first, &song);
        assert!(context.captured());
        assert_eq!(context.release_id().as_deref(), Some("r-1998"));

        // Captured once: a later track changes nothing.
        song.label = Some("Other".into());
        context.capture(&first, &song);
        assert_eq!(context.settled().label.as_deref(), Some("Virgin"));

        let mut second = plan(
            scored("r-2019", "G", "2019-08-23"),
            vec![
                scored("r-2019", "g", "2019-08-23"),
                scored("R-1998", "g", "1998-04-20"),
            ],
        );
        assert!(context.prefer_settled_release(&mut second));
        assert_eq!(
            second
                .chosen
                .as_ref()
                .and_then(|c| c.candidate.release_id.as_deref()),
            Some("R-1998")
        );
        assert_eq!(
            second.notes,
            ["the walk's release was kept over another pressing of the same album (2019-08-23)"]
        );
        // Already on the settled release: nothing to do.
        assert!(!context.prefer_settled_release(&mut second));

        let mut other_album = plan(
            scored("r-x", "other", "2000"),
            vec![scored("r-1998", "g", "1998")],
        );
        assert!(!context.prefer_settled_release(&mut other_album));

        let mut track = Song {
            album: "Mezzanine".into(),
            year: None,
            label: Some("Peer label".into()),
            ..Default::default()
        };
        context.pin(&mut track);
        assert_eq!(track.label.as_deref(), Some("Virgin"));
        assert_eq!(track.catalog_number.as_deref(), Some("CDV 2851"));
        assert_eq!(track.year, Some(1998));
        assert_eq!(track.total_tracks, Some(11));
        assert_eq!(track.music_brainz_release_id.as_deref(), Some("r-1998"));

        let mut elsewhere = Song {
            album: "Another".into(),
            ..Default::default()
        };
        context.pin(&mut elsewhere);
        assert_eq!(elsewhere.label, None);
    }

    #[test]
    fn nothing_is_captured_from_a_weak_plan_or_another_album() {
        let context: AlbumTagContext = AlbumTagContext::new(None, "Mezzanine", None);
        let weak = TagPlan {
            confidence: TagConfidence::Low,
            ..plan(scored("r", "g", "1998"), vec![])
        };
        let song = Song {
            album: "Mezzanine".into(),
            ..Default::default()
        };
        context.capture(&weak, &song);
        assert!(!context.captured());
        let other = Song {
            album: "Collected".into(),
            ..Default::default()
        };
        context.capture(&plan(scored("r", "g", "1998"), vec![]), &other);
        assert!(!context.captured());
    }

    #[test]
    fn loudness_is_keyed_ignoring_case() {
        let context: AlbumTagContext<f64> = AlbumTagContext::new(None, "A", None);
        context.set_loudness("/m/A/01.flac", Some(-9.0));
        context.set_loudness("/m/a/01.FLAC", Some(-8.0));
        context.set_loudness("/m/A/02.flac", None);
        assert_eq!(
            context.loudness(),
            [
                ("/m/A/01.flac".to_string(), Some(-8.0)),
                ("/m/A/02.flac".to_string(), None)
            ]
        );
    }
}
