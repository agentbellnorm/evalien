//! Colors and symbols for the terminal.

use std::{io::IsTerminal, sync::OnceLock};

use crate::domain::Timestamp;

/// Colors only for a person at a terminal; logs (a pod's, say) stay plain.
fn paint(code: &str, text: &str) -> String {
    static COLORS: OnceLock<bool> = OnceLock::new();
    let colors = *COLORS.get_or_init(|| {
        std::io::stdout().is_terminal() && std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
    });
    if colors {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

pub fn dim(text: &str) -> String {
    paint("90", text)
}

pub fn red(text: &str) -> String {
    paint("31", text)
}

pub fn green(text: &str) -> String {
    paint("32", text)
}

pub fn yellow(text: &str) -> String {
    paint("33", text)
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
