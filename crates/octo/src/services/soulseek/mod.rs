//! `Services/Soulseek`: the parts with I/O or shared state. The pure parts (the routing model,
//! song lengths, search profiles) are in `octo_core::soulseek`.

pub mod external_id_registry;
pub mod radio_queue_store;
pub mod rejected_peer_registry;
pub mod soulseek_link;
pub mod soulseek_metadata_service;

pub use external_id_registry::{ExternalIdRegistry, SharedRouting};
pub use radio_queue_store::RadioQueueStore;
pub use rejected_peer_registry::RejectedPeerRegistry;
pub use soulseek_link::ISoulseekLink;
pub use soulseek_metadata_service::SoulseekMetadataService;
