use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime},
};

use crate::domain::{Entry, Event, FinishReason, Generation, Outcome, Seq, Usd};

/// Quality bars for a run. A failing check exits nonzero.
pub struct Thresholds {
    /// Share of evaluations whose source doesn't parse as JavaScript.
    pub malformed_rate: f64,
    /// Longest a human waits before a model call sees their input.
    pub stdin_wait: Duration,
    /// Share of input tokens served from cache, once a run has a few calls.
    pub cache_read_share: f64,
}

pub const THRESHOLDS: Thresholds = Thresholds {
    malformed_rate: 0.05,
    stdin_wait: Duration::from_secs(60),
    cache_read_share: 0.5,
};

#[derive(Debug, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

#[derive(Debug, PartialEq)]
pub struct Report {
    pub summary: Vec<(&'static str, String)>,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.pass)
    }
}

/// Entries from the latest start on.
pub fn last_run(entries: &[Entry]) -> &[Entry] {
    let start = entries
        .iter()
        .rposition(|entry| entry.event == Event::Start)
        .unwrap_or(0);
    &entries[start..]
}

/// A model call and where it sits in the trajectory.
struct Call<'a> {
    seq: Seq,
    timestamp: SystemTime,
    through: Seq,
    generation: &'a Generation,
}

impl Call<'_> {
    /// When the call started: it was recorded once it finished.
    fn started(&self) -> SystemTime {
        self.timestamp
            .checked_sub(self.generation.latency)
            .unwrap_or(self.timestamp)
    }
}

/// Summarize a run and check it against [`THRESHOLDS`]. `parses` decides
/// whether source is well-formed.
pub fn analyze(entries: &[Entry], parses: &dyn Fn(&str) -> bool) -> Report {
    let calls: Vec<Call<'_>> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Generation { through, generation } => Some(Call {
                seq: entry.seq,
                timestamp: entry.timestamp.as_system_time(),
                through: *through,
                generation,
            }),
            _ => None,
        })
        .collect();
    let evals: Vec<(Seq, &str)> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Eval { code } => Some((entry.seq, code.as_str())),
            _ => None,
        })
        .collect();
    let malformed: Vec<&(Seq, &str)> = evals.iter().filter(|(_, code)| !parses(code)).collect();
    let markup = malformed
        .iter()
        .filter(|(_, code)| code.contains("<invoke") || code.contains("```"))
        .count();
    let thrown = entries
        .iter()
        .filter(|e| {
            matches!(
                e.event,
                Event::Result {
                    outcome: Outcome::Throw,
                    ..
                }
            )
        })
        .count();

    let cost: Usd = calls.iter().map(|call| call.generation.usage.cost).sum();
    let cache_read: u64 = calls.iter().map(|call| call.generation.usage.cache_read_tokens).sum();
    let input_tokens: u64 = calls.iter().map(|call| call.generation.usage.input_tokens()).sum();
    let output_tokens: u64 = calls.iter().map(|call| call.generation.usage.output_tokens).sum();
    let incomplete = calls
        .iter()
        .filter(|call| call.generation.finish_reason != FinishReason::Stop)
        .count();
    // The runtime appends a call's eval right after the call, so a completed
    // call without one was an idle reply.
    let idle = calls
        .iter()
        .filter(|call| call.generation.finish_reason == FinishReason::Stop)
        .filter(|call| !evals.iter().any(|(seq, _)| seq.get() == call.seq.get() + 1))
        .count();

    // A call saw input once its prompt ran through the input's seq.
    let waits: Vec<(Seq, Option<Duration>)> = entries
        .iter()
        .filter(|entry| matches!(entry.event, Event::Stdin { .. }))
        .map(|entry| {
            let seen = calls.iter().find(|call| call.through >= entry.seq);
            let wait = seen.map(|call| {
                call.started()
                    .duration_since(entry.timestamp.as_system_time())
                    .unwrap_or(Duration::ZERO)
            });
            (entry.seq, wait)
        })
        .collect();
    let slow = waits
        .iter()
        .filter(|(_, wait)| wait.is_none_or(|w| w > THRESHOLDS.stdin_wait))
        .count();

    let malformed_rate = if evals.is_empty() {
        0.0
    } else {
        malformed.len() as f64 / evals.len() as f64
    };
    let mut parse_detail = format!(
        "{}/{} malformed ({}, max {})",
        malformed.len(),
        evals.len(),
        percent(malformed_rate),
        percent(THRESHOLDS.malformed_rate)
    );
    if !malformed.is_empty() {
        let seqs: Vec<String> = malformed.iter().map(|(seq, _)| seq.to_string()).collect();
        parse_detail.push_str(&format!("; seq {}", seqs.join(", ")));
    }
    if markup > 0 {
        parse_detail.push_str(&format!("; {markup} contain tool-call markup or fences"));
    }
    let wait_detail = if waits.is_empty() {
        "no input".to_owned()
    } else {
        let each: Vec<String> = waits
            .iter()
            .map(|(seq, wait)| match wait {
                Some(wait) => format!("seq {seq}: {:.1}s", wait.as_secs_f64()),
                None => format!("seq {seq}: never seen"),
            })
            .collect();
        format!("{} (max {}s)", each.join(", "), THRESHOLDS.stdin_wait.as_secs())
    };

    let mut checks = vec![
        Check {
            name: "model output parses as JavaScript",
            pass: malformed_rate <= THRESHOLDS.malformed_rate,
            detail: parse_detail,
        },
        Check {
            name: "human input reaches the model promptly",
            pass: slow == 0,
            detail: wait_detail,
        },
    ];
    if calls.len() >= 3 {
        let share = if input_tokens == 0 {
            0.0
        } else {
            cache_read as f64 / input_tokens as f64
        };
        checks.push(Check {
            name: "prompt cache is reused",
            pass: share >= THRESHOLDS.cache_read_share,
            detail: format!(
                "{} of input tokens read from cache (min {})",
                percent(share),
                percent(THRESHOLDS.cache_read_share)
            ),
        });
    }

    let span = match (entries.first(), entries.last()) {
        (Some(first), Some(last)) => format!("{} → {}", first.timestamp, last.timestamp),
        _ => "empty".to_owned(),
    };
    let mut summary = vec![
        ("events", entries.len().to_string()),
        ("span", span),
        ("evaluations", evals.len().to_string()),
        ("thrown results", thrown.to_string()),
        (
            "generations",
            format!("{} ({idle} idle, {incomplete} incomplete)", calls.len()),
        ),
    ];
    if !calls.is_empty() {
        let models: BTreeSet<&str> = calls.iter().map(|call| call.generation.model.as_str()).collect();
        let latency: Duration = calls.iter().map(|call| call.generation.latency).sum();
        let per_call = Usd::new(cost.get() / calls.len() as f64).unwrap_or(Usd::ZERO);
        summary.extend([
            ("models", models.into_iter().collect::<Vec<_>>().join(", ")),
            ("cost", format!("{cost} ({per_call} per call)")),
            ("input tokens", format!("{input_tokens} ({cache_read} cached)")),
            ("output tokens", output_tokens.to_string()),
            (
                "mean latency",
                format!("{}ms", latency.as_millis() / calls.len() as u128),
            ),
        ]);
    }
    Report { summary, checks }
}

pub fn render(report: &Report) -> String {
    let width = report.summary.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    let mut lines: Vec<String> = report
        .summary
        .iter()
        .map(|(key, value)| format!("{key:width$}  {value}"))
        .collect();
    lines.push(String::new());
    lines.extend(report.checks.iter().map(|check| {
        format!(
            "{}  {}: {}",
            if check.pass { "PASS" } else { "FAIL" },
            check.name,
            check.detail
        )
    }));
    lines.join("\n") + "\n"
}

fn percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}
