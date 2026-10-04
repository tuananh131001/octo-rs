//! The .NET framework pieces the ported services were built on, which have no Rust
//! counterpart with the same behaviour: `SlidingWindowRateLimiter`, a size-limited
//! `MemoryCache`, the `Lazy<Task<T>>` single-flight pattern, and a buffered `HttpClient` call.

pub mod http;
pub mod in_flight;
pub mod memory_cache;
pub mod sliding_window_rate_limiter;

pub use http::HttpAnswer;
pub use in_flight::InFlight;
pub use memory_cache::MemoryCache;
pub use sliding_window_rate_limiter::{
    RateLimitLease, SlidingWindowRateLimiter, SlidingWindowRateLimiterOptions,
};
