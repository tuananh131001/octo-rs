//! The application's shared state: one `Arc` per service, standing in for the singletons
//! `Program.cs` registered with ASP.NET's container.

use std::sync::Arc;

/// Cheap to clone; handlers and workers receive it by value.
#[derive(Clone, Default)]
pub struct AppState {
    pub inner: Arc<AppInner>,
}

/// The services. Filled in as each part of the port lands.
#[derive(Default)]
pub struct AppInner {}
