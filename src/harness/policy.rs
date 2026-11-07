//! When the model is called, and how often.

use std::time::Duration;

use crate::domain::Event;

/// The first few model calls without human input run back to back, so the
/// agent can work. Further calls get further apart until input arrives.
pub const IDLE_DELAYS: [Duration; 7] = [
    Duration::ZERO,
    Duration::ZERO,
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];

/// Whether an observation asks for a model call. Results and input always do.
/// Output does when nothing is pending; a pending evaluation's output is seen
/// with its result. Submitted source and model calls never do.
pub fn wakes(event: &Event, pending: usize) -> bool {
    match event {
        Event::Start | Event::Stdin { .. } | Event::Result { .. } => true,
        Event::Stdout { .. } | Event::Stderr { .. } => pending == 0,
        Event::Eval { .. } | Event::Generation { .. } => false,
    }
}

/// A fresh start or human input resets pacing.
pub fn from_human(event: &Event) -> bool {
    match event {
        Event::Start | Event::Stdin { .. } => true,
        Event::Eval { .. }
        | Event::Stdout { .. }
        | Event::Stderr { .. }
        | Event::Result { .. }
        | Event::Generation { .. } => false,
    }
}

/// Minimum spacing before the nth consecutive call without human input.
pub fn idle_delay(quiet: usize, delays: &[Duration]) -> Duration {
    if quiet == 0 || delays.is_empty() {
        return Duration::ZERO;
    }
    delays[quiet.min(delays.len()) - 1]
}
