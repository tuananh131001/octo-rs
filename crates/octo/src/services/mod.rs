//! The services: `Services/**` in the C#, the parts with I/O, state or wiring: stores, clients
//! and workers. Each submodule mirrors a C# namespace; the pure halves are in `octo_core` under
//! the same module paths.

pub mod admin;
pub mod common;
pub mod local;
pub mod lyrics;
pub mod metadata;
pub mod soulseek;
pub mod state_file;
pub mod updates;
pub mod you_tube;
