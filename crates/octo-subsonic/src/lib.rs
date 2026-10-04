//! The Subsonic wire format: response shapes in XML and JSON, request parsing, model mapping.

pub mod subsonic_credential;
pub mod subsonic_model_mapper;
pub mod subsonic_request_parser;
pub mod xml;

pub use subsonic_credential::SubsonicCredential;
pub use subsonic_request_parser::{Parameters, RequestParts, StringValuesCollection};
