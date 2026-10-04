//! Port of `Services/Soulseek/SearchProfile.cs`.

use crate::settings::SoulseekSettings;

/// How long and how wide one Soulseek search looks. slskd ends a search at whichever comes
/// first: SearchTimeoutMs with no new answer, ResponseLimit peers, or FileLimit files. The file
/// limit counts every format. Octo asked for 150 files, so a popular song's first wave of MP3s
/// ended the search before the FLAC answers arrived (#70). CeilingSeconds is Octo's own limit:
/// a search still running then is cancelled, and slskd still hands over what it gathered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchProfile {
    pub name: String,
    pub ceiling_seconds: i32,
    pub search_timeout_ms: i32,
    pub response_limit: i32,
    pub file_limit: i32,
}

impl SearchProfile {
    pub fn new(
        name: impl Into<String>,
        ceiling_seconds: i32,
        search_timeout_ms: i32,
        response_limit: i32,
        file_limit: i32,
    ) -> Self {
        SearchProfile {
            name: name.into(),
            ceiling_seconds,
            search_timeout_ms,
            response_limit,
            file_limit,
        }
    }

    /// A star or a play. Quality beats speed, but somebody may be waiting, so the
    /// configured ceiling holds. 500 files lets a popular song run past its MP3s; it usually
    /// ends at the ceiling now instead of after 5 to 20 s.
    pub fn interactive(s: &SoulseekSettings) -> Self {
        Self::new("interactive", s.search_wait_seconds, 15_000, 250, 500)
    }

    /// Better quality and the weekly upgrade. Nobody is waiting, and the quick search
    /// is the one that already came back without a lossless copy.
    pub fn upgrade(s: &SoulseekSettings) -> Self {
        Self::new(
            "upgrade",
            s.effective_upgrade_search_wait_seconds(),
            30_000,
            500,
            2_000,
        )
    }

    /// An album heart's one search for the whole record. Each peer answers with many
    /// files, so the file limit is wide; nobody is waiting on one song, so it may look a little
    /// longer than a star does.
    pub fn album(s: &SoulseekSettings) -> Self {
        Self::new("album", s.search_wait_seconds.max(45), 20_000, 500, 3_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile half of SoulseekSearchProfileTests.ThePayloadCarriesEachProfilesLimits; the
    /// payload half (`SoulseekClient.SearchPayload`) moves with the Soulseek client (4-A).
    #[test]
    fn each_profile_carries_its_limits() {
        let s = SoulseekSettings::default();
        let limits = |p: &SearchProfile| (p.search_timeout_ms, p.response_limit, p.file_limit);
        assert_eq!(limits(&SearchProfile::interactive(&s)), (15_000, 250, 500));
        assert_eq!(limits(&SearchProfile::upgrade(&s)), (30_000, 500, 2_000));
        assert_eq!(limits(&SearchProfile::album(&s)), (20_000, 500, 3_000));
        assert_eq!(
            (
                SearchProfile::interactive(&s).ceiling_seconds,
                SearchProfile::upgrade(&s).ceiling_seconds
            ),
            (30, 90)
        );
        let wide = SoulseekSettings {
            upgrade_search_wait_seconds: 9999,
            ..Default::default()
        };
        assert_eq!(SearchProfile::upgrade(&wide).ceiling_seconds, 300);
    }

    #[test]
    fn an_album_search_waits_at_least_45_seconds() {
        let short = SoulseekSettings {
            search_wait_seconds: 10,
            ..Default::default()
        };
        assert_eq!(SearchProfile::album(&short).ceiling_seconds, 45);
        let long = SoulseekSettings {
            search_wait_seconds: 60,
            ..Default::default()
        };
        assert_eq!(SearchProfile::album(&long).ceiling_seconds, 60);
        assert_eq!(SearchProfile::album(&long).name, "album");
    }
}
