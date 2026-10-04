//! STUB(2-B): replaced when 2-B lands with its port of `Services/Validation/ValidationResult.cs`
//! (ported in full here, since `SubsonicStartupValidator` (3-E) builds every kind).

use std::collections::HashMap;

/// `System.ConsoleColor`, the colours the validators use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleColor {
    DarkGray,
    Red,
    Green,
    Yellow,
    Cyan,
    #[default]
    White,
}

/// Result of a startup validation operation
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ValidationResult {
    /// Indicates whether the validation was successful
    pub is_valid: bool,
    /// Short status message (e.g., "VALID", "INVALID", "TIMEOUT", "NOT CONFIGURED")
    pub status: String,
    /// Detailed information about the validation result
    pub details: Option<String>,
    /// Color to use when displaying the status in console
    pub status_color: ConsoleColor,
    /// Additional metadata about the validation
    pub metadata: HashMap<String, serde_json::Value>,
}

impl ValidationResult {
    /// Creates a successful validation result
    pub fn success(details: impl Into<String>) -> Self {
        ValidationResult {
            is_valid: true,
            status: "VALID".into(),
            status_color: ConsoleColor::Green,
            details: Some(details.into()),
            metadata: HashMap::new(),
        }
    }

    /// Creates a failed validation result
    pub fn failure(status: impl Into<String>, details: impl Into<String>, color: ConsoleColor) -> Self {
        ValidationResult {
            is_valid: false,
            status: status.into(),
            status_color: color,
            details: Some(details.into()),
            metadata: HashMap::new(),
        }
    }

    /// Creates a not configured validation result
    pub fn not_configured(details: impl Into<String>) -> Self {
        Self::failure("NOT CONFIGURED", details, ConsoleColor::Red)
    }
}
