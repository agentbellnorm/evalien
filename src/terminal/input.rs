//! Human input, one nonblank line at a time.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::domain::{Event, Trajectory, TrajectoryError};

/// Append each nonblank line as input until the reader ends. Closing the
/// stream ends input only; the agent keeps running.
pub async fn forward_lines(reader: impl AsyncBufRead + Unpin, log: &Trajectory) -> Result<(), TrajectoryError> {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            log.append(Event::Stdin { text: line })?;
        }
    }
    Ok(())
}
