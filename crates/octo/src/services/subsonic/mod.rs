//! `Services/Subsonic`: the parts with app services. The wire format itself (the response
//! builder, the model mapper, the sync catalog page) is in `octo_subsonic`.

pub mod subsonic_response_builder;

pub use subsonic_response_builder::{SubsonicResponseBuilderExt, new_subsonic_response_builder};

#[cfg(test)]
mod subsonic_wire_golden_tests;
