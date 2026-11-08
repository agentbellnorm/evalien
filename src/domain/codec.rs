//! The stored form of events: canonical, versioned JSON. Version 1 is the
//! format the TypeScript runtime wrote, so existing databases stay readable.
//!
//! The wire types mirror the JSON exactly and reject anything else. Converting
//! them to domain types is where values get checked.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::event::{Event, FinishReason, Generation, Outcome, Seq, Usage, Usd};

const VERSION: u32 = 1;

pub fn encode(event: &Event) -> String {
    let envelope = Envelope {
        version: VERSION,
        event: WireEvent::from(event),
    };
    // Wire types hold only strings, integers and finite floats.
    serde_json::to_string(&envelope).expect("trajectory events always serialize")
}

pub fn decode(json: &str) -> Result<Event, String> {
    let envelope: Envelope = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if envelope.version != VERSION {
        return Err(format!("unsupported trajectory version: {}", envelope.version));
    }
    envelope.event.try_into()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    event: WireEvent,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum WireEvent {
    Start {},
    Eval {
        code: String,
    },
    Stdin {
        text: String,
    },
    Stdout {
        text: String,
    },
    Stderr {
        text: String,
    },
    Notice {
        text: String,
    },
    Result {
        #[serde(rename = "evalId")]
        eval_id: u64,
        outcome: WireOutcome,
        text: String,
    },
    Generation {
        through: u64,
        model: String,
        #[serde(rename = "latencyMs")]
        latency_ms: u64,
        #[serde(rename = "finishReason")]
        finish_reason: WireFinishReason,
        usage: WireUsage,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireOutcome {
    Return,
    Throw,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WireFinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Error,
    Other,
    Unknown,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireUsage {
    uncached_input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    #[serde(rename = "costUSD")]
    cost_usd: f64,
}

impl From<&Event> for WireEvent {
    fn from(event: &Event) -> Self {
        match event.clone() {
            Event::Start => WireEvent::Start {},
            Event::Eval { code } => WireEvent::Eval { code },
            Event::Stdin { text } => WireEvent::Stdin { text },
            Event::Stdout { text } => WireEvent::Stdout { text },
            Event::Stderr { text } => WireEvent::Stderr { text },
            Event::Notice { text } => WireEvent::Notice { text },
            Event::Result { eval, outcome, text } => WireEvent::Result {
                eval_id: eval.get(),
                outcome: match outcome {
                    Outcome::Return => WireOutcome::Return,
                    Outcome::Throw => WireOutcome::Throw,
                },
                text,
            },
            Event::Generation { through, generation } => WireEvent::Generation {
                through: through.get(),
                model: generation.model,
                latency_ms: u64::try_from(generation.latency.as_millis()).unwrap_or(u64::MAX),
                finish_reason: match generation.finish_reason {
                    FinishReason::Stop => WireFinishReason::Stop,
                    FinishReason::Length => WireFinishReason::Length,
                    FinishReason::ContentFilter => WireFinishReason::ContentFilter,
                    FinishReason::ToolCalls => WireFinishReason::ToolCalls,
                    FinishReason::Error => WireFinishReason::Error,
                    FinishReason::Other => WireFinishReason::Other,
                    FinishReason::Unknown => WireFinishReason::Unknown,
                },
                usage: WireUsage {
                    uncached_input_tokens: generation.usage.uncached_input_tokens,
                    cache_read_tokens: generation.usage.cache_read_tokens,
                    cache_write_tokens: generation.usage.cache_write_tokens,
                    output_tokens: generation.usage.output_tokens,
                    reasoning_tokens: generation.usage.reasoning_tokens,
                    cost_usd: generation.usage.cost.get(),
                },
            },
        }
    }
}

impl TryFrom<WireEvent> for Event {
    type Error = String;

    fn try_from(wire: WireEvent) -> Result<Self, String> {
        Ok(match wire {
            WireEvent::Start {} => Event::Start,
            WireEvent::Eval { code } => Event::Eval { code },
            WireEvent::Stdin { text } => Event::Stdin { text },
            WireEvent::Stdout { text } => Event::Stdout { text },
            WireEvent::Stderr { text } => Event::Stderr { text },
            WireEvent::Notice { text } => Event::Notice { text },
            WireEvent::Result { eval_id, outcome, text } => {
                if eval_id == 0 {
                    return Err("a result must reference a positive evaluation ID".into());
                }
                Event::Result {
                    eval: Seq::new(eval_id),
                    outcome: match outcome {
                        WireOutcome::Return => Outcome::Return,
                        WireOutcome::Throw => Outcome::Throw,
                    },
                    text,
                }
            }
            WireEvent::Generation {
                through,
                model,
                latency_ms,
                finish_reason,
                usage,
            } => Event::Generation {
                through: Seq::new(through),
                generation: Generation {
                    model,
                    latency: Duration::from_millis(latency_ms),
                    finish_reason: match finish_reason {
                        WireFinishReason::Stop => FinishReason::Stop,
                        WireFinishReason::Length => FinishReason::Length,
                        WireFinishReason::ContentFilter => FinishReason::ContentFilter,
                        WireFinishReason::ToolCalls => FinishReason::ToolCalls,
                        WireFinishReason::Error => FinishReason::Error,
                        WireFinishReason::Other => FinishReason::Other,
                        WireFinishReason::Unknown => FinishReason::Unknown,
                    },
                    usage: Usage {
                        uncached_input_tokens: usage.uncached_input_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_write_tokens: usage.cache_write_tokens,
                        output_tokens: usage.output_tokens,
                        reasoning_tokens: usage.reasoning_tokens,
                        cost: Usd::new(usage.cost_usd).ok_or("cost must be a nonnegative number")?,
                    },
                },
            },
        })
    }
}
