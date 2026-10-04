//! STUB(2-B): replaced when 2-B lands with its port of
//! `Services/Validation/BaseStartupValidator.cs`. Only the console helpers
//! `SubsonicStartupValidator` (3-E) writes with are here.
//!
//! .NET's `Console.ForegroundColor` changed the colour only on a terminal; with the output
//! redirected (a container's log) the text went out plain. The same here.

use std::io::{IsTerminal, Write};

use super::ConsoleColor;

fn ansi(color: ConsoleColor) -> &'static str {
    match color {
        ConsoleColor::DarkGray => "\x1b[90m",
        ConsoleColor::Red => "\x1b[91m",
        ConsoleColor::Green => "\x1b[92m",
        ConsoleColor::Yellow => "\x1b[93m",
        ConsoleColor::Cyan => "\x1b[96m",
        ConsoleColor::White => "\x1b[97m",
    }
}

fn colored(text: &str, color: ConsoleColor) -> String {
    if std::io::stdout().is_terminal() {
        format!("{}{text}\x1b[0m", ansi(color))
    } else {
        text.to_string()
    }
}

/// Writes a status line to the console with colored output
pub fn write_status(label: &str, value: &str, value_color: ConsoleColor) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "  {label}: {}", colored(value, value_color));
}

/// Writes a detail line to the console in dark gray
pub fn write_detail(message: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{}",
        colored(&format!("    -> {message}"), ConsoleColor::DarkGray)
    );
}
