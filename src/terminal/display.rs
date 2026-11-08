//! Shows the trajectory to the human as it grows.

use std::sync::Arc;

use crate::domain::{Entry, Event, Outcome, Seq, Trajectory};

use super::format::{ARROW, BOLT, CROSS, debug, dim, green, red, yellow};

/// Where a line goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Out(String),
    Err(String),
    /// A host diagnostic.
    Note(String),
}

/// Follow the trajectory from after `cursor`, printing each entry.
pub async fn show(log: Arc<Trajectory>, mut cursor: Seq) {
    loop {
        let latest = log.wait_after(cursor).await;
        match log.read(cursor, latest) {
            Ok(entries) => entries.iter().filter_map(line).for_each(print),
            Err(error) => debug(&format!("display: {error}")),
        }
        cursor = latest;
    }
}

fn print(line: Line) {
    match line {
        Line::Out(text) => println!("{text}"),
        Line::Err(text) => eprintln!("{text}"),
        Line::Note(text) => debug(&text),
    }
}

/// How an entry looks to the human, if it shows at all.
pub fn line(entry: &Entry) -> Option<Line> {
    match &entry.event {
        Event::Start => Some(Line::Out(format!(
            "evalien -- agent is waking up ({} prior events)...\n",
            entry.seq.get() - 1
        ))),
        Event::Stdin { text } => Some(Line::Out(green(&format!("[you] {text}")))),
        Event::Eval { code } => Some(Line::Out(dim(&format!("{BOLT} {}", code.replace('\n', "\n  "))))),
        Event::Stdout { text } => Some(Line::Out(text.clone())),
        Event::Stderr { text } => Some(Line::Err(text.clone())),
        Event::Notice { text } => Some(Line::Err(format!("[evalien] {text}"))),
        Event::Result {
            outcome: Outcome::Throw,
            text,
            ..
        } => Some(Line::Err(red(&format!("{CROSS} {text}")))),
        Event::Result {
            outcome: Outcome::Return,
            text,
            ..
        } if text == "undefined" => None,
        Event::Result {
            outcome: Outcome::Return,
            text,
            ..
        } => Some(Line::Out(yellow(&format!("{ARROW} {text}")))),
        Event::Generation { generation, .. } => {
            let usage = &generation.usage;
            Some(Line::Note(format!(
                "{} {}ms {} | {} in, {} cached, {} out, {}",
                generation.model,
                generation.latency.as_millis(),
                generation.finish_reason.as_str(),
                usage.input_tokens(),
                usage.cache_read_tokens,
                usage.output_tokens,
                usage.cost,
            )))
        }
    }
}
