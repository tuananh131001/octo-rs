//! The services: `Services/**` in the C#, the parts with I/O, state or wiring. Each submodule
//! mirrors a C# namespace; `framework` holds the .NET library pieces they were built on.

pub mod admin;
pub mod common;
pub mod cover_art;
pub mod fingerprint;
pub mod framework;
pub mod local;
pub mod metadata;
pub mod soulseek;
pub mod state_file;
pub mod updates;
pub mod you_tube;

#[cfg(test)]
pub(crate) mod test_support;
