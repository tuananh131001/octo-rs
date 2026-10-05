//! `UseExceptionHandler(_ => {})` + `GlobalExceptionHandler` for failures handlers do not
//! return: a panic anywhere in a handler becomes the same 500 JSON envelope an unclassified
//! exception produced in C#, instead of a dropped connection.
//!
//! Handlers that fail on purpose return `Err(AppError)`, which renders itself
//! (`http::error`). This layer only catches panics while the response is being produced; a
//! panic while a streaming body is being written cannot be turned into a status any more, as
//! in ASP.NET once the response had started.
//!
//! It sits inside the CORS layer, so the 500 still carries the CORS headers, as the C# error
//! responses did (CORS applied its headers when the response started).

use std::panic::AssertUnwindSafe;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use futures::FutureExt;

use crate::http::error::panic_response;
use crate::workers::supervisor::panic_message;

pub async fn catch_panic(req: Request, next: Next) -> Response {
    match AssertUnwindSafe(next.run(req)).catch_unwind().await {
        Ok(res) => res,
        Err(payload) => panic_response(&panic_message(payload)),
    }
}
