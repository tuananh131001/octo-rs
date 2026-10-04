//! A typed error with a code, a message and metadata. Port of `Services/Common/Error.cs`.
//!
//! The C# `Result<T>` and `Result` (`Services/Common/Result.cs`) are plain
//! `Result<T, Error>` and `Result<(), Error>` here: `IsSuccess` is `is_ok()`, `Value` is
//! `ok()`, and the implicit conversions are `Ok(..)` and `Err(..)`.

use std::collections::HashMap;
use std::fmt;

/// Categorizes error types for appropriate HTTP status code mapping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorType {
    /// Validation error (400 Bad Request)
    Validation,

    /// Resource not found (404 Not Found)
    NotFound,

    /// Authentication required (401 Unauthorized)
    Unauthorized,

    /// Insufficient permissions (403 Forbidden)
    Forbidden,

    /// Resource conflict (409 Conflict)
    Conflict,

    /// Internal server error (500 Internal Server Error)
    Internal,

    /// External service error (502 Bad Gateway / 503 Service Unavailable)
    ExternalService,
}

/// Additional metadata about an error.
pub type ErrorMetadata = HashMap<String, serde_json::Value>;

/// Represents a typed error with code, message, and metadata
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    /// Unique error code identifier
    pub code: String,

    /// Human-readable error message
    pub message: String,

    /// Error type/category
    pub error_type: ErrorType,

    /// Additional metadata about the error
    pub metadata: Option<ErrorMetadata>,
}

impl Error {
    fn new(code: String, message: String, error_type: ErrorType, metadata: Option<ErrorMetadata>) -> Self {
        Self {
            code,
            message,
            error_type,
            metadata,
        }
    }

    fn with_default_code(
        default_code: &str,
        message: impl Into<String>,
        code: Option<&str>,
        error_type: ErrorType,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::new(
            code.unwrap_or(default_code).to_string(),
            message.into(),
            error_type,
            metadata,
        )
    }

    /// Creates a Not Found error (404)
    pub fn not_found(
        message: impl Into<String>,
        code: Option<&str>,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::with_default_code("NOT_FOUND", message, code, ErrorType::NotFound, metadata)
    }

    /// Creates a Validation error (400)
    pub fn validation(
        message: impl Into<String>,
        code: Option<&str>,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::with_default_code("VALIDATION_ERROR", message, code, ErrorType::Validation, metadata)
    }

    /// Creates an Unauthorized error (401)
    pub fn unauthorized(
        message: impl Into<String>,
        code: Option<&str>,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::with_default_code("UNAUTHORIZED", message, code, ErrorType::Unauthorized, metadata)
    }

    /// Creates a Forbidden error (403)
    pub fn forbidden(
        message: impl Into<String>,
        code: Option<&str>,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::with_default_code("FORBIDDEN", message, code, ErrorType::Forbidden, metadata)
    }

    /// Creates a Conflict error (409)
    pub fn conflict(message: impl Into<String>, code: Option<&str>, metadata: Option<ErrorMetadata>) -> Self {
        Self::with_default_code("CONFLICT", message, code, ErrorType::Conflict, metadata)
    }

    /// Creates an Internal Server Error (500)
    pub fn internal(message: impl Into<String>, code: Option<&str>, metadata: Option<ErrorMetadata>) -> Self {
        Self::with_default_code("INTERNAL_ERROR", message, code, ErrorType::Internal, metadata)
    }

    /// Creates an External Service Error (502/503)
    pub fn external_service(
        message: impl Into<String>,
        code: Option<&str>,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::with_default_code(
            "EXTERNAL_SERVICE_ERROR",
            message,
            code,
            ErrorType::ExternalService,
            metadata,
        )
    }

    /// Creates a custom error with specified type
    pub fn custom(
        code: impl Into<String>,
        message: impl Into<String>,
        error_type: ErrorType,
        metadata: Option<ErrorMetadata>,
    ) -> Self {
        Self::new(code.into(), message.into(), error_type, metadata)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factories_fill_the_default_codes() {
        let cases = [
            (
                Error::not_found("m", None, None),
                "NOT_FOUND",
                ErrorType::NotFound,
            ),
            (
                Error::validation("m", None, None),
                "VALIDATION_ERROR",
                ErrorType::Validation,
            ),
            (
                Error::unauthorized("m", None, None),
                "UNAUTHORIZED",
                ErrorType::Unauthorized,
            ),
            (
                Error::forbidden("m", None, None),
                "FORBIDDEN",
                ErrorType::Forbidden,
            ),
            (Error::conflict("m", None, None), "CONFLICT", ErrorType::Conflict),
            (
                Error::internal("m", None, None),
                "INTERNAL_ERROR",
                ErrorType::Internal,
            ),
            (
                Error::external_service("m", None, None),
                "EXTERNAL_SERVICE_ERROR",
                ErrorType::ExternalService,
            ),
        ];
        for (error, code, error_type) in cases {
            assert_eq!(error.code, code);
            assert_eq!(error.error_type, error_type);
            assert_eq!(error.to_string(), "m");
        }
        assert_eq!(
            Error::not_found("m", Some("SONG_NOT_FOUND"), None).code,
            "SONG_NOT_FOUND"
        );
        assert_eq!(Error::custom("X", "m", ErrorType::Conflict, None).code, "X");
    }
}
