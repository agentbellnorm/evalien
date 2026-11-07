//! How the trajectory becomes prompt text.

use crate::domain::{Entry, Event, Seq, Trajectory, TrajectoryError};

/// Larger bodies keep their head and tail; the full event stays in the trajectory.
pub const MAX_ENTRY_CHARS: usize = 8_000;
/// Bounds input cost per call, even when evaluated code floods output.
pub const MAX_PROMPT_CHARS: usize = 400_000;
/// At least this many recent events, until the window's start moves a step.
const WINDOW_EVENTS: u64 = 500;
/// The window's start moves in steps of this many events, so prompts share a cached prefix.
const WINDOW_STEP: u64 = 200;

/// Each entry renders independently, so appending preserves the existing text.
pub fn render_entry(entry: &Entry) -> String {
    let (label, body) = match &entry.event {
        Event::Start => (
            "start".to_owned(),
            "Fresh runtime. Previous live bindings, callbacks, and connections are gone.".to_owned(),
        ),
        Event::Eval { code } => ("eval".to_owned(), code.clone()),
        Event::Stdin { text } => ("stdin".to_owned(), text.clone()),
        Event::Stdout { text } => ("stdout".to_owned(), text.clone()),
        Event::Stderr { text } => ("stderr".to_owned(), text.clone()),
        Event::Result { eval, outcome, text } => (format!("result eval={eval} {}", outcome.as_str()), text.clone()),
        Event::Generation { through, generation } => {
            let u = &generation.usage;
            let body = format!(
                "{} through={through}: {:.1}s, {}, in {} + {} cached + {} written, out {} ({} reasoning), {}",
                generation.model,
                generation.latency.as_secs_f64(),
                generation.finish_reason.as_str(),
                u.uncached_input_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.output_tokens,
                u.reasoning_tokens,
                u.cost,
            );
            ("generation".to_owned(), body)
        }
    };
    let framed: Vec<String> = clip(entry.seq, &body)
        .split('\n')
        .map(|line| format!("| {line}"))
        .collect();
    let framed = framed.join("\n");
    format!("[{} {} {label}]\n{framed}\n\n", entry.seq, entry.timestamp)
}

pub fn render_trajectory(entries: &[Entry]) -> String {
    entries.iter().map(render_entry).collect()
}

/// A recent window whose start moves in fixed steps, so consecutive prompts
/// share a prefix. Only an oversized window drops older entries beyond that.
pub fn prompt_window(log: &Trajectory, through: Seq) -> Result<Vec<String>, TrajectoryError> {
    let through_n = through.get();
    let mut after = through_n.saturating_sub(WINDOW_EVENTS) / WINDOW_STEP * WINDOW_STEP;
    let mut blocks = render_range(log, after, through)?;
    while after + WINDOW_STEP < through_n && size(&blocks) > MAX_PROMPT_CHARS {
        after += WINDOW_STEP;
        blocks = render_range(log, after, through)?;
    }
    let mut total = size(&blocks);
    let mut first = 0;
    while blocks.len() - first > 1 && total > MAX_PROMPT_CHARS {
        total -= blocks[first].chars().count();
        first += 1;
    }
    Ok(blocks.split_off(first))
}

fn render_range(log: &Trajectory, after: u64, through: Seq) -> Result<Vec<String>, TrajectoryError> {
    Ok(log.read(Seq::new(after), through)?.iter().map(render_entry).collect())
}

fn size(blocks: &[String]) -> usize {
    blocks.iter().map(|block| block.chars().count()).sum()
}

fn clip(seq: Seq, body: &str) -> String {
    let length = body.chars().count();
    if length <= MAX_ENTRY_CHARS {
        return body.to_owned();
    }
    let half = MAX_ENTRY_CHARS / 2;
    let head: String = body.chars().take(half).collect();
    let tail: String = body.chars().skip(length - half).collect();
    let omitted = length - MAX_ENTRY_CHARS;
    format!("{head}\n… {omitted} characters omitted; full text in trajectory seq {seq} …\n{tail}")
}
