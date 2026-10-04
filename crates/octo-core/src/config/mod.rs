//! Configuration: the layered sources, the binder, and the live settings store.

pub mod bind;
pub mod tree;

pub use bind::{BindWarning, bind, bind_strict};
pub use tree::{ConfigNode, ConfigTree};
