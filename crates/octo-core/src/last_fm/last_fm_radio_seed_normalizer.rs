//! Port of `Services/LastFm/LastFmRadioSeedNormalizer.cs`.
//!
//! Canonicalizes artist/title seeds for every Last.fm radio path, by [`SongIdentity`]'s reading
//! of a credit and a title.

use crate::common::SongIdentity;
use crate::common::dotnet;

/// The primary artist, as written: "Beyoncé" for "Beyoncé feat. Jay-Z", and "Tyler, The
/// Creator" or "Simon & Garfunkel" whole. A blank value comes back as it was.
pub fn artist(artist: &str) -> String {
    if dotnet::is_blank(artist) {
        return artist.to_string();
    }
    SongIdentity::primary_artist(artist).trim().to_string()
}

/// The title without its guest credits. A blank value comes back as it was.
pub fn title(title: &str) -> String {
    if dotnet::is_blank(title) {
        return title.to_string();
    }
    SongIdentity::strip_features(title)
}

/// One song in one version, however its artist and title are written.
pub fn track_key(artist: &str, title: &str) -> String {
    SongIdentity::match_key(artist, title)
}

#[cfg(test)]
mod tests {
    use super::*;

    // LastFmRadioCoreTests.SeedNormalizer_RemovesFeatureDecorationsWithoutDamagingArtist
    #[test]
    fn seed_normalizer_removes_feature_decorations_without_damaging_artist() {
        for (input, expected) in [
            ("Beyoncé feat. Jay-Z", "Beyoncé"),
            ("Run the Jewels ft Killer Mike", "Run the Jewels"),
            ("AC/DC", "AC/DC"),
        ] {
            assert_eq!(artist(input), expected, "{input}");
        }
    }

    // LastFmRadioCoreTests.SeedNormalizer_CleansCommonTitleDecorations
    #[test]
    fn seed_normalizer_cleans_common_title_decorations() {
        for (input, expected) in [
            ("Song (feat. Guest)", "Song"),
            ("Song [featuring Guest]", "Song"),
            ("Song (with Guest)", "Song"),
        ] {
            assert_eq!(title(input), expected, "{input}");
        }
    }

    #[test]
    fn blank_values_come_back_as_they_were() {
        assert_eq!(artist("  "), "  ");
        assert_eq!(title(""), "");
    }
}
