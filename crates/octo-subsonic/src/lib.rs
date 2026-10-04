//! The Subsonic wire format: response shapes in XML and JSON, request parsing, model mapping.

pub mod subsonic_model_mapper;
pub mod subsonic_response_builder;
pub mod sync_catalog_response;
pub mod xml;

pub use subsonic_model_mapper::{Row, SubsonicModelMapper};
pub use subsonic_response_builder::{IdRegistry, ReplyKind, SubsonicReply, SubsonicResponseBuilder};
