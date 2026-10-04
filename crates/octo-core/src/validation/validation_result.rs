//! Port of `Services/Validation/ValidationResult.cs`.

use indexmap::IndexMap;

/// `System.ConsoleColor`, which the startup report colours its statuses with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConsoleColor {
    Black,
    DarkBlue,
    DarkGreen,
    DarkCyan,
    DarkRed,
    DarkMagenta,
    DarkYellow,
    Gray,
    DarkGray,
    Blue,
    Green,
    Cyan,
    Red,
    Magenta,
    Yellow,
    #[default]
    White,
}

impl ConsoleColor {
    /// The ANSI foreground code .NET's terminal driver sets for this colour.
    pub fn ansi_code(self) -> u8 {
        match self {
            ConsoleColor::Black => 30,
            ConsoleColor::DarkRed => 31,
            ConsoleColor::DarkGreen => 32,
            ConsoleColor::DarkYellow => 33,
            ConsoleColor::DarkBlue => 34,
            ConsoleColor::DarkMagenta => 35,
            ConsoleColor::DarkCyan => 36,
            ConsoleColor::Gray => 37,
            ConsoleColor::DarkGray => 90,
            ConsoleColor::Red => 91,
            ConsoleColor::Green => 92,
            ConsoleColor::Yellow => 93,
            ConsoleColor::Blue => 94,
            ConsoleColor::Magenta => 95,
            ConsoleColor::Cyan => 96,
            ConsoleColor::White => 97,
        }
    }
}

/// A metadata value (`object` in C#).
pub type MetadataValue = serde_json::Value;

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
    pub metadata: IndexMap<String, MetadataValue>,
}

impl ValidationResult {
    /// Creates a successful validation result
    pub fn success(details: impl Into<String>, metadata: Option<IndexMap<String, MetadataValue>>) -> Self {
        Self {
            is_valid: true,
            status: "VALID".to_string(),
            status_color: ConsoleColor::Green,
            details: Some(details.into()),
            metadata: metadata.unwrap_or_default(),
        }
    }

    /// Creates a failed validation result. (C#'s default colour is red.)
    pub fn failure(status: impl Into<String>, details: impl Into<String>, color: ConsoleColor) -> Self {
        Self {
            is_valid: false,
            status: status.into(),
            status_color: color,
            details: Some(details.into()),
            metadata: IndexMap::new(),
        }
    }

    /// Creates a not configured validation result
    pub fn not_configured(details: impl Into<String>) -> Self {
        Self::failure("NOT CONFIGURED", details, ConsoleColor::Red)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_factories_set_status_and_colour() {
        let ok = ValidationResult::success("fine", None);
        assert!(ok.is_valid);
        assert_eq!(ok.status, "VALID");
        assert_eq!(ok.status_color, ConsoleColor::Green);
        assert_eq!(ok.details.as_deref(), Some("fine"));

        let missing = ValidationResult::not_configured("Subsonic URL not configured");
        assert!(!missing.is_valid);
        assert_eq!(missing.status, "NOT CONFIGURED");
        assert_eq!(missing.status_color, ConsoleColor::Red);

        let blank = ValidationResult::default();
        assert_eq!(blank.status, "");
        assert_eq!(blank.status_color, ConsoleColor::White);
    }
}
