//! The services: `Services/**` in the C#, the parts with I/O.

pub mod cover_art;
pub mod fingerprint;
pub mod framework;
pub mod metadata;

#[cfg(test)]
pub(crate) mod test_support;
