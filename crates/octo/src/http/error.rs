//! What an unhandled failure looks like on the wire: the port of `GlobalExceptionHandler`.
//!
//! In C#, any exception that escaped a controller became a Subsonic error envelope in JSON,
//! whatever `f` asked for, on every route including the admin API, with the HTTP status and
//! Subsonic code chosen by the exception's type. Handlers here return `Result<_, AppError>`,
//! and `AppError`'s variants stand for those exception types so the mapping stays the same.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use octo_core::json::format::{Escaping, Options, to_string_with};
use serde_json::json;

/// The Subsonic protocol version Octo reports in every envelope it builds.
pub const SUBSONIC_VERSION: &str = "1.16.1";

/// A failure that escaped a handler, named after the .NET exception it replaces.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// `OctoNotConfiguredException`: no Navidrome URL. The message tells the user what to set,
    /// so it goes out verbatim.
    #[error("{0}")]
    NotConfigured(String),
    /// `FileNotFoundException`.
    #[error("file not found: {0}")]
    FileNotFound(String),
    /// `DirectoryNotFoundException`.
    #[error("directory not found: {0}")]
    DirectoryNotFound(String),
    /// `UnauthorizedAccessException`.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// `ArgumentNullException`.
    #[error("missing argument: {0}")]
    ArgumentNull(String),
    /// `ArgumentException`.
    #[error("invalid argument: {0}")]
    Argument(String),
    /// `FormatException`.
    #[error("invalid format: {0}")]
    Format(String),
    /// `InvalidOperationException`.
    #[error("invalid operation: {0}")]
    InvalidOperation(String),
    /// `HttpRequestException`: an upstream that could not be reached or answered with a
    /// failure the caller chose to throw on.
    #[error("upstream request failed: {0}")]
    Http(String),
    /// `TimeoutException`. Note that an HttpClient timeout in .NET is a
    /// `TaskCanceledException`, not this, and so mapped to 500; see [`AppError::from_reqwest`].
    #[error("timed out: {0}")]
    Timeout(String),
    /// Anything else.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    /// The HTTP status, Subsonic error code and message, in `MapExceptionToResponse`'s order.
    pub fn mapping(&self) -> (StatusCode, i32, String) {
        match self {
            AppError::NotConfigured(m) => (StatusCode::SERVICE_UNAVAILABLE, 0, m.clone()),
            AppError::FileNotFound(_) => (StatusCode::NOT_FOUND, 70, "Resource not found".into()),
            AppError::DirectoryNotFound(_) => (StatusCode::NOT_FOUND, 70, "Directory not found".into()),
            AppError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, 40, "Wrong username or password".into()),
            AppError::ArgumentNull(_) => (
                StatusCode::BAD_REQUEST,
                10,
                "Required parameter is missing".into(),
            ),
            AppError::Argument(_) => (StatusCode::BAD_REQUEST, 10, "Invalid request".into()),
            AppError::Format(_) => (StatusCode::BAD_REQUEST, 10, "Invalid format".into()),
            AppError::InvalidOperation(_) => (StatusCode::BAD_REQUEST, 10, "Operation not valid".into()),
            AppError::Http(_) => (StatusCode::BAD_GATEWAY, 0, "External service unavailable".into()),
            AppError::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, 0, "Request timeout".into()),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                0,
                "An internal server error occurred".into(),
            ),
        }
    }

    /// How a failed `HttpClient` call surfaced in C#: a connection or status failure was an
    /// `HttpRequestException` (502), while a timeout was a `TaskCanceledException`, which the
    /// handler did not single out (500).
    pub fn from_reqwest(e: reqwest::Error) -> AppError {
        if e.is_timeout() {
            AppError::Internal(anyhow::Error::new(e).context("the request timed out"))
        } else {
            AppError::Http(e.to_string())
        }
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::from_reqwest(e)
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => AppError::FileNotFound(e.to_string()),
            std::io::ErrorKind::PermissionDenied => AppError::Unauthorized(e.to_string()),
            _ => AppError::Internal(e.into()),
        }
    }
}

impl From<octo_core::json::web::WebError> for AppError {
    fn from(e: octo_core::json::web::WebError) -> Self {
        AppError::Format(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = ?self, "Unhandled exception occurred: {self}");
        let (status, code, message) = self.mapping();
        let body = json!({
            "subsonic-response": {
                "status": "failed",
                "version": SUBSONIC_VERSION,
                "error": { "code": code, "message": message },
            }
        });
        // WriteAsJsonAsync after ContentType = "application/json": ASP.NET keeps the type it was
        // given and appends the charset. WriteAsJsonAsync uses the minimal-API JSON options,
        // whose encoder is UnsafeRelaxedJsonEscaping (the em dash in the not-configured message
        // goes out as is, unlike MVC's JsonResult).
        let mut res = json_relaxed_response(status, &body, "application/json; charset=utf-8");
        // ExceptionHandlerMiddleware marks the error response uncacheable.
        let headers = res.headers_mut();
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache,no-store"),
        );
        headers.insert(header::EXPIRES, HeaderValue::from_static("-1"));
        headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        res
    }
}

/// The response for a handler that panicked: what `GlobalExceptionHandler` made of an
/// exception it did not single out (500, code 0, "An internal server error occurred").
pub fn panic_response(message: &str) -> Response {
    AppError::Internal(anyhow::anyhow!("{message}")).into_response()
}

/// A JSON body as `HttpResponse.WriteAsJsonAsync` writes it: the minimal-API serializer
/// options, whose encoder is `UnsafeRelaxedJsonEscaping` (non-ASCII and `'` `<` `>` `&` `+`
/// left as they are).
pub fn json_relaxed_response(
    status: StatusCode,
    body: &impl serde::Serialize,
    content_type: &'static str,
) -> Response {
    let options = Options {
        escaping: Escaping::Relaxed,
        indented: false,
    };
    let mut res = (status, to_string_with(body, options)).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

/// A JSON body written the way System.Text.Json writes it (default encoder escaping), with an
/// explicit content type.
pub fn json_response(
    status: StatusCode,
    body: &impl serde::Serialize,
    content_type: &'static str,
) -> Response {
    let mut res = (status, octo_core::json::to_string(body)).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

/// `Ok(new { ... })` / `JsonResult` from a controller: STJ output, `application/json; charset=utf-8`.
pub fn json_ok(body: &impl serde::Serialize) -> Response {
    json_response(StatusCode::OK, body, "application/json; charset=utf-8")
}

/// A JSON body with a status, as `StatusCode(n, new { ... })` / `BadRequest(new { error })`.
pub fn json_status(status: StatusCode, body: &impl serde::Serialize) -> Response {
    json_response(status, body, "application/json; charset=utf-8")
}

/// What `[ApiController]` turns a bodiless client-error result (`NotFound()`, `BadRequest()`,
/// `StatusCode(4xx)`) into: an RFC 7807 problem document.
pub fn problem(status: StatusCode) -> Response {
    // A status ASP.NET has no ClientErrorMapping for (502, 503, ...) gets neither a type nor a
    // title: the factory leaves both null and STJ's ProblemDetails converter skips nulls.
    let body = match problem_type(status) {
        Some((kind, title)) => json!({
            "type": kind,
            "title": title,
            "status": status.as_u16(),
            "traceId": trace_id(),
        }),
        None => json!({ "status": status.as_u16(), "traceId": trace_id() }),
    };
    // A ProblemDetails is an ObjectResult, written by SystemTextJsonOutputFormatter, which
    // swaps the default encoder for UnsafeRelaxedJsonEscaping when none is configured.
    json_relaxed_response(status, &body, "application/problem+json; charset=utf-8")
}

/// The type URI and title ASP.NET's ProblemDetailsFactory gives a status (its
/// `ClientErrorMapping` defaults).
fn problem_type(status: StatusCode) -> Option<(&'static str, &'static str)> {
    Some(match status.as_u16() {
        400 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.1",
            "Bad Request",
        ),
        401 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.2",
            "Unauthorized",
        ),
        403 => ("https://tools.ietf.org/html/rfc9110#section-15.5.4", "Forbidden"),
        404 => ("https://tools.ietf.org/html/rfc9110#section-15.5.5", "Not Found"),
        405 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.6",
            "Method Not Allowed",
        ),
        406 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.7",
            "Not Acceptable",
        ),
        408 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.9",
            "Request Timeout",
        ),
        409 => ("https://tools.ietf.org/html/rfc9110#section-15.5.10", "Conflict"),
        412 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.13",
            "Precondition Failed",
        ),
        415 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.16",
            "Unsupported Media Type",
        ),
        422 => (
            "https://tools.ietf.org/html/rfc4918#section-11.2",
            "Unprocessable Entity",
        ),
        426 => (
            "https://tools.ietf.org/html/rfc9110#section-15.5.22",
            "Upgrade Required",
        ),
        500 => (
            "https://tools.ietf.org/html/rfc9110#section-15.6.1",
            "An error occurred while processing your request.",
        ),
        _ => return None,
    })
}

/// The automatic `400` `[ApiController]` answers when model validation fails before the
/// action runs (`ValidationProblemDetails`): `errors` maps each field to its messages.
pub fn validation_problem(errors: &[(&str, &[&str])]) -> Response {
    let mut map = serde_json::Map::new();
    for (field, messages) in errors {
        map.insert((*field).to_string(), json!(messages));
    }
    let body = json!({
        "type": "https://tools.ietf.org/html/rfc9110#section-15.5.1",
        "title": "One or more validation errors occurred.",
        "status": 400,
        "errors": map,
        "traceId": trace_id(),
    });
    json_relaxed_response(
        StatusCode::BAD_REQUEST,
        &body,
        "application/problem+json; charset=utf-8",
    )
}

/// ASP.NET's trace id: the W3C `00-<trace>-<span>-00` form of the current activity. Values are
/// random per request, so the parity harness treats the field as volatile.
fn trace_id() -> String {
    let trace: u128 = rand::random();
    let span: u64 = rand::random();
    format!("00-{trace:032x}-{span:016x}-00")
}

/// Shorthand for handler results.
pub type AppResult<T = Response> = Result<T, AppError>;
