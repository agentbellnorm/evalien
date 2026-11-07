//! Colors and symbols for the terminal.

use crate::domain::Timestamp;

const RESET: &str = "\x1b[0m";

pub fn dim(text: &str) -> String {
    format!("\x1b[90m{text}{RESET}")
}

pub fn red(text: &str) -> String {
    format!("\x1b[31m{text}{RESET}")
}

pub fn green(text: &str) -> String {
    format!("\x1b[32m{text}{RESET}")
}

pub fn yellow(text: &str) -> String {
    format!("\x1b[33m{text}{RESET}")
}

pub const BOLT: &str = "\u{26a1}";
pub const CROSS: &str = "\u{2718}";
pub const ARROW: &str = "\u{2192}";

/// A host diagnostic, with the time of day. Never part of the trajectory.
pub fn debug(message: &str) {
    let now = Timestamp::now().to_string();
    let time = now.get(11..19).unwrap_or(&now);
    eprintln!("{}", dim(&format!("[{time}] {message}")));
}
