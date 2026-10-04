//! The data half of `Services/Fingerprint/AcoustIdClient.cs`: the records an AcoustID lookup
//! answers with. `VerificationResult` and the release chooser hold them, so they live here; the
//! HTTP client, its rate limiter and `ParseLookup` belong to the `octo` crate's port of the
//! client (task 2-D), which builds these records.

/// One credited artist and the text MusicBrainz joins it to the next one with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdCredit {
    pub name: String,
    pub artist_id: Option<String>,
    pub join_phrase: String,
}

impl AcoustIdCredit {
    pub fn new(name: impl Into<String>, artist_id: Option<&str>, join_phrase: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            artist_id: artist_id.map(str::to_string),
            join_phrase: join_phrase.into(),
        }
    }
}

/// The release a recording was matched on. It supplies what names the album, numbers the
/// track and finds the cover, and it is chosen per recording by PickRelease.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdRelease {
    pub release_id: Option<String>,
    pub release_group_id: Option<String>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub track_count: Option<i32>,
    pub disc_number: Option<i32>,
    pub album_artist: Option<String>,
    pub is_compilation: bool,

    /// The rest of what the service says about a release, kept for every release of
    /// every group so the chooser can weigh them: the group's title and kind, the release's own
    /// title, date and country, the track's own id, and the album artists' ids.
    pub group_title: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub country: Option<String>,
    pub date: Option<String>,
    pub release_track_id: Option<String>,
    pub disc_count: Option<i32>,
    pub album_artist_ids: Vec<String>,
}

/// One recording AcoustID matched, with the MusicBrainz fields that come back in the same
/// lookup. There is no separate MusicBrainz client on purpose: AcoustID's metadata IS
/// MusicBrainz data, and asking for it via meta= costs nothing extra on a call already
/// being made and already inside a rate budget.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdRecording {
    pub recording_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album_title: Option<String>,
    pub year: Option<i32>,

    pub credits: Vec<AcoustIdCredit>,
    pub release: Option<AcoustIdRelease>,
    pub duration_seconds: Option<i32>,

    /// Every release of every group the recording is on, bounded, for the chooser.
    /// [`release`](Self::release) stays the one pick the old fields are read from.
    pub releases: Vec<AcoustIdRelease>,

    /// The codes the music database lists for the recording, normalised.
    pub isrcs: Vec<String>,

    /// How many submissions tie the fingerprint to this recording.
    pub sources: i32,
}

impl AcoustIdRecording {
    /// The positional part of the C# record; everything else starts empty.
    pub fn new<S: Into<String>>(
        recording_id: impl Into<String>,
        title: impl Into<String>,
        artists: impl IntoIterator<Item = S>,
        album_title: Option<&str>,
        year: Option<i32>,
    ) -> Self {
        Self {
            recording_id: recording_id.into(),
            title: title.into(),
            artists: artists.into_iter().map(Into::into).collect(),
            album_title: album_title.map(str::to_string),
            year,
            ..Default::default()
        }
    }

    /// The credit as MusicBrainz prints it, join phrases and all. Never a bare comma join:
    /// Navidrome does not split artists on commas, so "Bizarrap, Rauw Alejandro" became one
    /// artist and one folder that neither of them owns (#49).
    pub fn artist_credit(&self) -> String {
        if self.credits.is_empty() {
            Self::join_names(&self.artists)
        } else {
            Self::join_credits(&self.credits)
        }
    }

    /// The first credited artist: the one a folder is named after.
    pub fn primary_artist(&self) -> Option<&str> {
        match self.credits.first() {
            Some(credit) => Some(&credit.name),
            None => self.artists.first().map(String::as_str),
        }
    }

    pub fn join_credits(credits: &[AcoustIdCredit]) -> String {
        let mut builder = String::new();
        for (i, credit) in credits.iter().enumerate() {
            builder.push_str(&credit.name);
            if i == credits.len() - 1 {
                break;
            }
            // compress drops a join phrase the parent level already carries, so a missing one
            // is read the way MusicBrainz most often prints it.
            let join = credit.join_phrase.as_str();
            builder.push_str(if !join.is_empty() {
                join
            } else if i == credits.len() - 2 {
                " & "
            } else {
                ", "
            });
        }
        builder
    }

    pub fn join_names(names: &[String]) -> String {
        match names {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} & {last}", rest.join(", ")),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdResult {
    pub score: f64,
    pub recordings: Vec<AcoustIdRecording>,
    /// The service's own id for the fingerprint, written to a confirmed file.
    pub id: Option<String>,
}

impl AcoustIdResult {
    pub fn new(score: f64, recordings: Vec<AcoustIdRecording>) -> Self {
        Self {
            score,
            recordings,
            id: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdLookup {
    pub is_ok: bool,
    pub error: Option<String>,
    pub results: Vec<AcoustIdResult>,
}

impl AcoustIdLookup {
    pub fn new(is_ok: bool, error: Option<&str>, results: Vec<AcoustIdResult>) -> Self {
        Self {
            is_ok,
            error: error.map(str::to_string),
            results,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_credit_joins_with_the_phrases_or_the_usual_ones() {
        let credited = AcoustIdRecording {
            credits: vec![
                AcoustIdCredit::new("Massive Attack", Some("a1"), " feat. "),
                AcoustIdCredit::new("Elizabeth Fraser", Some("a2"), ""),
            ],
            ..AcoustIdRecording::new("r", "Teardrop", ["Massive Attack"], None, None)
        };
        assert_eq!(credited.artist_credit(), "Massive Attack feat. Elizabeth Fraser");
        assert_eq!(credited.primary_artist(), Some("Massive Attack"));

        let three = [
            AcoustIdCredit::new("A", None, ""),
            AcoustIdCredit::new("B", None, ""),
            AcoustIdCredit::new("C", None, ""),
        ];
        assert_eq!(AcoustIdRecording::join_credits(&three), "A, B & C");

        let named = AcoustIdRecording::new("r", "t", ["A", "B", "C"], None, None);
        assert_eq!(named.artist_credit(), "A, B & C");
        assert_eq!(named.primary_artist(), Some("A"));
        assert_eq!(AcoustIdRecording::default().artist_credit(), "");
        assert_eq!(AcoustIdRecording::default().primary_artist(), None);
    }
}
