//! Soulseek: the pure parts of `Services/Soulseek` (the routing model, the shown-length rules
//! and the search profiles). The clients, stores and services that do I/O live in
//! `octo::services::soulseek`.

pub mod search_profile;
pub mod song_length;
pub mod soulseek_metadata_service;

pub use search_profile::SearchProfile;
pub use song_length::{LengthSource, SongLength};
pub use soulseek_metadata_service::{RoutingKind, SoulseekRouting};
