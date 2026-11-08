//! Talking to the human through a chat service. A [`Chat`] receives the
//! human's messages and sends messages back; [`relay`] connects one to the
//! trajectory. Messages in become `stdin`; the agent's `console.log` output
//! and the runtime's notices go out. The harness doesn't know chat exists.

mod matrix;

use std::{future::Future, sync::Arc, time::Duration};

use crate::domain::{Entry, Event, Seq, Trajectory};

pub use matrix::{Matrix, MatrixConfig};

/// Longest message to send, in bytes. Longer output goes out in parts.
const MAX_MESSAGE: usize = 30_000;
/// How long to let a burst of output land before sending it as one message.
const COALESCE: Duration = Duration::from_millis(300);
const SEND_ATTEMPTS: u32 = 5;

/// Who a message is from, so the chat can show the two differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// The agent's own output.
    Agent,
    /// The runtime, such as a paused budget.
    Runtime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The same across retries and restarts, so a resend never duplicates.
    pub key: String,
    pub speaker: Speaker,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ChatError(pub String);

pub trait Chat: Send + Sync {
    /// Wait for the human's next messages, oldest first.
    fn receive(&self) -> impl Future<Output = Result<Vec<String>, ChatError>> + Send;
    fn send(&self, message: &Outgoing) -> impl Future<Output = Result<(), ChatError>> + Send;
}

/// Connect a chat to the trajectory, from now on. Runs until the process ends.
pub async fn relay(chat: impl Chat, log: Arc<Trajectory>, debug: fn(&str)) {
    let from = log.last_seq();
    tokio::join!(inbound(&chat, &log, debug), outbound(&chat, &log, from, debug));
}

/// The human's messages become input.
async fn inbound(chat: &impl Chat, log: &Trajectory, debug: fn(&str)) {
    let mut failures = 0;
    loop {
        match chat.receive().await {
            Ok(messages) => {
                failures = 0;
                for text in messages {
                    if let Err(error) = log.append(Event::Stdin { text }) {
                        debug(&format!("chat: message not recorded: {error}"));
                    }
                }
            }
            Err(error) => {
                failures += 1;
                debug(&format!("chat: receiving failed: {error}"));
                tokio::time::sleep(backoff(failures)).await;
            }
        }
    }
}

/// Output and notices after `cursor` become messages.
async fn outbound(chat: &impl Chat, log: &Trajectory, mut cursor: Seq, debug: fn(&str)) {
    loop {
        log.wait_after(cursor).await;
        tokio::time::sleep(COALESCE).await;
        let latest = log.last_seq();
        let entries = match log.read(cursor, latest) {
            Ok(entries) => entries,
            Err(error) => {
                debug(&format!("chat: {error}"));
                continue;
            }
        };
        cursor = latest;
        for message in messages(&entries) {
            send(chat, &message, debug).await;
        }
    }
}

async fn send(chat: &impl Chat, message: &Outgoing, debug: fn(&str)) {
    for attempt in 1..=SEND_ATTEMPTS {
        match chat.send(message).await {
            Ok(()) => return,
            Err(error) if attempt == SEND_ATTEMPTS => {
                debug(&format!("chat: gave up sending {}: {error}", message.key));
            }
            Err(error) => {
                debug(&format!("chat: sending {} failed, retrying: {error}", message.key));
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

/// What a batch of entries says to the human, in order. Consecutive output
/// is joined into one message; each notice is its own.
pub fn messages(entries: &[Entry]) -> Vec<Outgoing> {
    let mut messages = Vec::new();
    let mut output: Option<(Seq, String)> = None;
    for entry in entries {
        match &entry.event {
            Event::Stdout { text } => {
                let (_, joined) = output.get_or_insert_with(|| (entry.seq, String::new()));
                if !joined.is_empty() {
                    joined.push('\n');
                }
                joined.push_str(text.trim_end_matches('\n'));
            }
            Event::Notice { text } => {
                flush(&mut messages, output.take());
                push(&mut messages, entry.seq, Speaker::Runtime, text);
            }
            _ => {}
        }
    }
    flush(&mut messages, output.take());
    messages
}

fn flush(messages: &mut Vec<Outgoing>, output: Option<(Seq, String)>) {
    if let Some((seq, text)) = output {
        push(messages, seq, Speaker::Agent, &text);
    }
}

/// Add a message, in parts if it's too long. Keys come from the first entry's
/// sequence number, which no other message shares.
fn push(messages: &mut Vec<Outgoing>, seq: Seq, speaker: Speaker, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    for (part, chunk) in chunks(text, MAX_MESSAGE).into_iter().enumerate() {
        messages.push(Outgoing {
            key: format!("evalien-{seq}-{part}"),
            speaker,
            text: chunk.to_owned(),
        });
    }
}

/// Split text into pieces of at most `max` bytes, on character boundaries,
/// preferring to break after a newline.
fn chunks(text: &str, max: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let end = rest[..end].rfind('\n').map_or(end, |newline| newline + 1);
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces.push(rest);
    pieces
}

/// 1s, 2s, 4s … up to a minute.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}
