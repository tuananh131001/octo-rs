//! The services: `Services/**` in the C#, the parts with I/O, state or wiring. Each submodule
//! mirrors a C# namespace.

pub mod admin;
pub mod common;
pub mod http_client_factory;
pub mod i_download_service;
pub mod library;
pub mod local;
pub mod soulseek;
pub mod state_file;
pub mod subsonic;
pub mod updates;
pub mod validation;
pub mod you_tube;
