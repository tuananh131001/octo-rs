//! `Services/Subsonic` in the C#: talking to Navidrome. The pure wire format (request parsing,
//! credentials) lives in `octo_subsonic`.

pub mod credential_check;
pub(crate) mod expiring_cache;
pub mod navidrome_identity_service;
pub mod recent_scrobbles;
pub mod request_identity;
pub mod search_budget;
pub mod search_song_order;
pub mod search_song_page_planner;
pub mod subsonic_discovery_service;
pub mod subsonic_proxy_service;

pub use credential_check::{CredentialCheck, CredentialVerdict};
pub use navidrome_identity_service::{NavidromeIdentityService, NavidromeLibrary};
pub use recent_scrobbles::RecentScrobbles;
pub use request_identity::RequestIdentity;
pub use search_budget::SearchBudget;
pub use search_song_order::{SearchSongOrder, SearchSongOrderCache};
pub use search_song_page_planner::{SearchSongPage, SearchSongPagePlanner};
pub use subsonic_discovery_service::{DiscoveredServer, SubsonicDiscoveryService};
pub use subsonic_proxy_service::{
    IncomingRequest, RawRelayResult, RelayError, RelayResponse, SubsonicProxyService,
};
