//! The request pipeline, outermost first, in `Program.cs` order (endpoints.md §2.1):
//!
//! 1. [`request_log`]: the hosting layer's "Request starting/finished" lines.
//! 2. [`forwarded`]: `UseForwardedHeaders()` (only `X-Forwarded-Proto`).
//! 3. [`admin_guard`]: `UseAdminRequestGuard()`, outside CORS so it strips CORS headers.
//! 4. [`cors`]: `UseCors()` with the default policy.
//! 5. [`exception`]: `UseExceptionHandler` + `GlobalExceptionHandler` for panics, inside CORS
//!    so error responses keep the CORS headers as they did in C#.
//! 6. Path canonicalisation, then the routes (static files are routes, as `MapStaticAssets`
//!    endpoints were), falling back to the catch-all.
//!
//! The C# raw-body middleware (`HttpContext.Items["Octo.RawBody"]` for every POST, PUT and
//! PATCH) has no counterpart: an axum handler that needs the raw body extracts it as `Bytes`
//! and parses the form or JSON from those bytes itself, so the bytes are still there for the
//! faithful relay.

pub mod admin_guard;
pub mod cors;
pub mod exception;
pub mod forwarded;
pub mod request_log;
