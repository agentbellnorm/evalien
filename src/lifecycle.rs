//! Stopping on signals. As PID 1 in a container the process has no default
//! signal handling, so Ctrl+C and `podman stop` only work through this.

use std::fmt;

use tokio::signal::unix::{SignalKind, signal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Interrupt,
    Terminate,
}

impl Signal {
    /// The conventional exit status: 128 plus the signal number.
    pub fn exit_code(self) -> u8 {
        match self {
            Signal::Interrupt => 130,
            Signal::Terminate => 143,
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Signal::Interrupt => "SIGINT",
            Signal::Terminate => "SIGTERM",
        })
    }
}

/// Wait for SIGINT or SIGTERM.
pub async fn stop_signal() -> std::io::Result<Signal> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => Ok(Signal::Interrupt),
        _ = terminate.recv() => Ok(Signal::Terminate),
    }
}
