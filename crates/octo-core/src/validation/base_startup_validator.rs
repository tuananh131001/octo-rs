//! Port of `Services/Validation/BaseStartupValidator.cs`.
//!
//! The C# abstract class held the `HttpClient` and gave its subclasses static helpers. A Rust
//! validator implements [`super::IStartupValidator`], keeps its own HTTP client, and calls these
//! helpers. Colour codes are written only when standard output is a terminal, as .NET's console
//! did (a container's redirected output got none).

use std::io::{IsTerminal, Write};

use super::validation_result::{ConsoleColor, ValidationResult};

/// What kind of failure [`BaseStartupValidator::handle_exception`] is describing: the C#
/// matched on the exception's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExceptionKind {
    /// `TaskCanceledException`: the request timed out (or was cancelled).
    Canceled,
    /// `HttpRequestException`: the service could not be reached.
    HttpRequest,
    /// Anything else.
    Other,
}

/// Base class for startup validators providing common functionality
pub struct BaseStartupValidator;

impl BaseStartupValidator {
    /// Writes a status line to the console with colored output
    pub fn write_status(label: &str, value: &str, value_color: ConsoleColor) {
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "  {label}: ");
        let _ = writeln!(out, "{}", colored(value, value_color));
    }

    /// Writes a detail line to the console in dark gray
    pub fn write_detail(message: &str) {
        let line = format!("    -> {message}");
        let _ = writeln!(
            std::io::stdout().lock(),
            "{}",
            colored(&line, ConsoleColor::DarkGray)
        );
    }

    /// Masks a secret string for display, showing only the first few characters
    pub fn mask_secret(secret: &str) -> String {
        if secret.is_empty() {
            return "(empty)".to_string();
        }

        // Lengths in UTF-16 units, as `string.Length` counted.
        const VISIBLE_CHARS: usize = 4;
        let units: Vec<u16> = secret.encode_utf16().collect();
        if units.len() <= VISIBLE_CHARS {
            return "*".repeat(units.len());
        }

        let visible = String::from_utf16_lossy(&units[..VISIBLE_CHARS]);
        visible + &"*".repeat((units.len() - VISIBLE_CHARS).min(8))
    }

    /// Handles common HTTP exceptions and returns appropriate validation result
    pub fn handle_exception(kind: ExceptionKind, message: &str, _field_name: &str) -> ValidationResult {
        match kind {
            ExceptionKind::Canceled => ValidationResult::failure(
                "TIMEOUT",
                "Could not reach service within timeout period",
                ConsoleColor::Yellow,
            ),
            ExceptionKind::HttpRequest => {
                ValidationResult::failure("UNREACHABLE", message, ConsoleColor::Yellow)
            }
            ExceptionKind::Other => ValidationResult::failure("ERROR", message, ConsoleColor::Red),
        }
    }

    /// Writes validation result to console
    pub fn write_validation_result(field_name: &str, result: &ValidationResult) {
        Self::write_status(field_name, &result.status, result.status_color);
        if let Some(details) = result.details.as_deref().filter(|details| !details.is_empty()) {
            Self::write_detail(details);
        }
    }
}

fn colored(text: &str, color: ConsoleColor) -> String {
    if std::io::stdout().is_terminal() {
        format!("\u{1b}[{}m{text}\u{1b}[39m", color.ansi_code())
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_shows_four_characters_and_at_most_eight_stars() {
        assert_eq!(BaseStartupValidator::mask_secret(""), "(empty)");
        assert_eq!(BaseStartupValidator::mask_secret("abc"), "***");
        assert_eq!(BaseStartupValidator::mask_secret("abcd"), "****");
        assert_eq!(BaseStartupValidator::mask_secret("abcdef"), "abcd**");
        assert_eq!(
            BaseStartupValidator::mask_secret("abcdefghijklmnopqrstuvwxyz"),
            "abcd********"
        );
    }

    #[test]
    fn handle_exception_names_the_kind_of_failure() {
        let timeout = BaseStartupValidator::handle_exception(ExceptionKind::Canceled, "ignored", "Url");
        assert_eq!(timeout.status, "TIMEOUT");
        assert_eq!(
            timeout.details.as_deref(),
            Some("Could not reach service within timeout period")
        );
        assert_eq!(timeout.status_color, ConsoleColor::Yellow);

        let unreachable =
            BaseStartupValidator::handle_exception(ExceptionKind::HttpRequest, "refused", "Url");
        assert_eq!(unreachable.status, "UNREACHABLE");
        assert_eq!(unreachable.details.as_deref(), Some("refused"));

        let other = BaseStartupValidator::handle_exception(ExceptionKind::Other, "boom", "Url");
        assert_eq!(other.status, "ERROR");
        assert_eq!(other.status_color, ConsoleColor::Red);
        assert!(!other.is_valid);
    }
}
