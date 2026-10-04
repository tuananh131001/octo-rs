//! Port of `Services/Common/PlaylistIdHelper.cs`.
//!
//! Helper for handling external playlist IDs.
//! Playlist IDs use the format: "pl-{provider}-{externalId}"
//! Example: "pl-deezer-123456", "pl-qobuz-789"

use super::dotnet::{eq_ignore_case, starts_with_ignore_case, to_lower_invariant};

const PLAYLIST_PREFIX: &str = "pl-";

/// The providers Octo can actually resolve a playlist from. (Compared ignoring case.)
///
/// Single source of truth: PlaylistSyncService asks here before mapping a provider onto
/// a metadata service, so there is one list rather than two that have to agree.
const KNOWN_PROVIDERS: [&str; 2] = ["deezer", "qobuz"];

/// What `ParsePlaylistId` and `CreatePlaylistId` threw (`ArgumentException`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidPlaylistId(pub String);

/// Checks whether a provider name is one we have a client for.
///
/// `provider`: the provider name, e.g. "deezer". Returns true if Octo can resolve playlists
/// from this provider.
pub fn is_known_provider(provider: Option<&str>) -> bool {
    provider.is_some_and(|provider| {
        !provider.is_empty()
            && KNOWN_PROVIDERS
                .iter()
                .any(|known| eq_ignore_case(known, provider))
    })
}

/// Checks if an ID represents an external playlist.
///
/// The provider has to be one we recognise, not merely present. Navidrome names its own
/// playlist cover art "pl-{id}_{unixhex}", which carries the same "pl-" prefix, so
/// matching on the prefix alone claimed every Navidrome playlist cover, failed to parse
/// a provider out of it, and served Octo's placeholder instead of relaying the request
/// upstream (issue #43).
///
/// Requiring a KNOWN provider rather than a second dash keeps that fixed whatever
/// Navidrome decides its ids look like. A shape-only check would work today only because
/// Navidrome ids happen to be base62 with no dashes, which is not ours to rely on.
///
/// Returns true if the ID is "pl-{knownProvider}-{externalId}", false otherwise.
pub fn is_external_playlist(id: Option<&str>) -> bool {
    try_parse_playlist_id(id).is_some()
}

/// Parses a playlist ID to extract provider and external ID.
///
/// `id`: the playlist ID in format "pl-{provider}-{externalId}". Returns (provider,
/// externalId), or an error if the ID format is invalid.
pub fn parse_playlist_id(id: &str) -> Result<(String, String), InvalidPlaylistId> {
    try_parse_playlist_id(Some(id)).ok_or_else(|| {
        InvalidPlaylistId(format!(
            "Invalid playlist ID format. Expected 'pl-{{provider}}-{{externalId}}', got '{id}' (Parameter 'id')"
        ))
    })
}

/// The one place the format is decided, so the predicate and the parser can never
/// disagree about what counts as an external playlist ID.
fn try_parse_playlist_id(id: Option<&str>) -> Option<(String, String)> {
    let id = id.filter(|id| !id.is_empty())?;
    if !starts_with_ignore_case(id, PLAYLIST_PREFIX) {
        return None;
    }

    // Remove "pl-" prefix (three ASCII characters, whatever their case)
    let without_prefix = &id[PLAYLIST_PREFIX.len()..];

    // Split by first dash to get provider and externalId
    let dash_index = without_prefix.find('-')?;
    if dash_index == 0 || dash_index == without_prefix.len() - 1 {
        return None;
    }

    let candidate_provider = &without_prefix[..dash_index];
    if !is_known_provider(Some(candidate_provider)) {
        return None;
    }

    Some((
        candidate_provider.to_string(),
        without_prefix[dash_index + 1..].to_string(),
    ))
}

/// Creates a playlist ID from provider and external ID.
///
/// `provider`: the provider name (e.g., "deezer", "qobuz"); `external_id`: the external ID
/// from the provider. Returns a playlist ID in format "pl-{provider}-{externalId}".
pub fn create_playlist_id(provider: &str, external_id: &str) -> Result<String, InvalidPlaylistId> {
    if provider.is_empty() {
        return Err(InvalidPlaylistId(
            "Provider cannot be null or empty (Parameter 'provider')".to_string(),
        ));
    }

    if external_id.is_empty() {
        return Err(InvalidPlaylistId(
            "External ID cannot be null or empty (Parameter 'externalId')".to_string(),
        ));
    }

    Ok(format!(
        "{PLAYLIST_PREFIX}{}-{external_id}",
        to_lower_invariant(provider)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // IsExternalPlaylist decides whether Octo answers a request itself or relays it to
    // Navidrome, so getting it wrong is not a missing feature, it is Octo overwriting a
    // perfectly good upstream response with its own.

    #[test]
    fn is_external_playlist_known_provider_ids_are_external() {
        for id in [
            "pl-deezer-123456",
            "pl-qobuz-789",
            "PL-DEEZER-123456",
            "pl-Deezer-abc-def",
        ] {
            assert!(is_external_playlist(Some(id)), "{id}");
        }
    }

    /// Navidrome names its playlist cover art "pl-{id}_{unixhex}", which collides with the
    /// prefix Octo uses for its own playlist IDs. Claiming those served the Octo placeholder
    /// instead of the user's own uploaded cover, in every client that goes through Octo.
    #[test]
    fn is_external_playlist_navidrome_cover_art_ids_are_not_external() {
        for id in [
            "pl-upVzEcScsvwZRAVYjePvu5_6aa92874",
            "pl-pAbMYHyXyD92FjOkSNiJBn_4fa08e4a",
        ] {
            assert!(!is_external_playlist(Some(id)), "{id}");
        }
    }

    /// The case a "require a second dash" fix misses. Navidrome's IDs are base62 today, so
    /// a dash cannot appear in one, but that is a property of someone else's ID generator
    /// and not something Octo gets to depend on. Checking the PROVIDER holds either way.
    #[test]
    fn is_external_playlist_navidrome_shaped_id_containing_a_dash_is_still_not_external() {
        for id in [
            "pl-upVz-EcScsvwZRAVYjePvu5_6aa92874",
            "pl-1a2b3c4d-5e6f-7890-abcd-ef1234567890",
        ] {
            assert!(!is_external_playlist(Some(id)), "{id}");
        }
    }

    #[test]
    fn is_external_playlist_malformed_or_unknown_provider_is_not_external() {
        for id in [
            Some("pl-"),
            Some("pl--123"),
            Some("pl-deezer-"),
            Some("pl-deezer"),
            Some("pl-spotify-123"),
        ]
        .into_iter()
        .chain([Some("al-123"), Some(""), None])
        {
            assert!(!is_external_playlist(id), "{id:?}");
        }
    }

    #[test]
    fn parse_playlist_id_splits_on_the_first_dash_after_the_provider() {
        let (provider, external_id) = parse_playlist_id("pl-deezer-abc-def").expect("valid");

        assert_eq!(provider, "deezer");
        assert_eq!(external_id, "abc-def");
    }

    #[test]
    fn parse_playlist_id_navidrome_cover_art_id_throws() {
        assert!(parse_playlist_id("pl-upVzEcScsvwZRAVYjePvu5_6aa92874").is_err());
    }

    #[test]
    fn create_playlist_id_round_trips_through_parse() {
        let id = create_playlist_id("Deezer", "123456").expect("valid");

        assert_eq!(id, "pl-deezer-123456");
        assert!(is_external_playlist(Some(&id)));
        assert_eq!(
            parse_playlist_id(&id).expect("valid"),
            ("deezer".to_string(), "123456".to_string())
        );
    }

    #[test]
    fn is_known_provider_matches_only_providers_we_have_a_client_for() {
        for (provider, expected) in [
            (Some("deezer"), true),
            (Some("qobuz"), true),
            (Some("DEEZER"), true),
            (Some("spotify"), false),
            (Some(""), false),
            (None, false),
        ] {
            assert_eq!(is_known_provider(provider), expected, "{provider:?}");
        }
    }

    #[test]
    fn create_playlist_id_refuses_empty_parts() {
        assert!(create_playlist_id("", "1").is_err());
        assert!(create_playlist_id("deezer", "").is_err());
    }
}
