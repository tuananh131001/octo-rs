//! `Services/Soulseek`: the parts with I/O or shared state. The pure parts (the routing model,
//! song lengths, search profiles, slskd's records, the candidate matching and the album folder
//! picker) are in `octo_core::soulseek`.

pub mod external_id_registry;
pub mod radio_queue_store;
pub mod rejected_peer_registry;
pub mod soulseek_client;
pub mod soulseek_download_service;
pub mod soulseek_link;
pub mod soulseek_metadata_service;
pub mod soulseek_startup_validator;

pub use external_id_registry::{ExternalIdRegistry, SharedRouting};
pub use octo_core::soulseek::{AlbumFolderChoice, AlbumFolderPicker, AlbumTrack};
pub use radio_queue_store::RadioQueueStore;
pub use rejected_peer_registry::RejectedPeerRegistry;
pub use soulseek_client::{SoulseekClient, SoulseekClientError};
pub use soulseek_link::{ISoulseekLink, SoulseekLink};
pub use soulseek_metadata_service::{LastFmTrackLengths, SoulseekMetadataService};
pub use soulseek_startup_validator::SoulseekStartupValidator;
