use std::{collections::HashMap, process::Command, time::Duration};

use evalien::db::{self, Access, SqliteStore};
use evalien::domain::{Entry, Event, FinishReason, Generation, Outcome, Seq, TRAJECTORY_TABLE, Trajectory, Usage, Usd};
use evalien::evaluation::SandboxHost;
use evalien::quality::{Report, analyze};

/// 100 × $2 + 900 × $0.20 + 50 × $10 per million is $0.00088 per call.
fn usage() -> Usage {
    Usage {
        uncached_input_tokens: 100,
        cache_read_tokens: 900,
        cache_write_tokens: 0,
        output_tokens: 50,
        reasoning_tokens: 0,
        cost: Usd::new(0.00088).unwrap(),
    }
}

fn call(through: Seq, finish_reason: FinishReason) -> Event {
    Event::Generation {
        through,
        generation: Generation {
            model: "anthropic/test".into(),
            latency: Duration::ZERO,
            finish_reason,
            usage: usage(),
        },
    }
}

enum Step {
    Generate,
    Eval(&'static str),
    /// Settles the latest eval.
    Settle(Outcome, &'static str),
    Other(Event),
}

/// Record a run as the runtime does: each call is appended right before its eval.
fn record(log: &Trajectory, steps: Vec<Step>) {
    let mut last_eval = Seq::ZERO;
    for step in steps {
        match step {
            Step::Generate => {
                log.append(call(log.last_seq(), FinishReason::Stop)).unwrap();
            }
            Step::Eval(code) => last_eval = log.append(Event::Eval { code: code.into() }).unwrap().seq,
            Step::Settle(outcome, text) => {
                log.append(Event::Result {
                    eval: last_eval,
                    outcome,
                    text: text.into(),
                })
                .unwrap();
            }
            Step::Other(event) => {
                log.append(event).unwrap();
            }
        }
    }
}

async fn check(entries: &[Entry]) -> Report {
    let mut checker = SandboxHost::new().unwrap().checker().await.unwrap();
    let mut parsed = HashMap::new();
    for entry in entries {
        if let Event::Eval { code } = &entry.event {
            parsed.insert(code.clone(), checker.parses(code).await.unwrap());
        }
    }
    analyze(entries, &|code| parsed[code])
}

fn summary<'a>(report: &'a Report, key: &str) -> &'a str {
    &report.summary.iter().find(|(k, _)| *k == key).unwrap().1
}

fn memory_log() -> Trajectory {
    Trajectory::new(evalien::db::MemoryStore::new()).unwrap()
}

#[tokio::test]
async fn a_healthy_run_passes_every_check() {
    let log = memory_log();
    record(
        &log,
        vec![
            Step::Other(Event::Start),
            Step::Generate,
            Step::Eval("return 1;"),
            Step::Settle(Outcome::Return, "1"),
            Step::Other(Event::Stdin { text: "hi".into() }),
            Step::Generate,
            Step::Eval("console.log(\"hello\")"),
            Step::Other(Event::Stdout { text: "hello\n".into() }),
            Step::Settle(Outcome::Return, "undefined"),
            Step::Generate,
            Step::Eval("JSON.parse('{')"),
            Step::Settle(Outcome::Throw, "SyntaxError: bad JSON"),
        ],
    );
    let report = check(&log.read_all().unwrap()).await;
    let checks: Vec<(&str, bool)> = report.checks.iter().map(|c| (c.name, c.pass)).collect();
    assert_eq!(
        checks,
        [
            ("model output parses as JavaScript", true),
            ("human input reaches the model promptly", true),
            ("prompt cache is reused", true),
        ]
    );
    // A SyntaxError thrown at runtime is the program's business, not malformed output.
    assert!(report.checks[0].detail.starts_with("0/3 malformed"));
    assert_eq!(summary(&report, "cost"), "$0.0026 ($0.0009 per call)");
    assert_eq!(summary(&report, "generations"), "3 (0 idle, 0 incomplete)");
}

#[tokio::test]
async fn idle_and_incomplete_calls_are_told_apart() {
    let log = memory_log();
    record(
        &log,
        vec![
            Step::Other(Event::Start),
            Step::Generate,
            Step::Eval("return 1;"),
            Step::Settle(Outcome::Return, "1"),
            Step::Generate,
            Step::Other(Event::Stdin { text: "hi".into() }),
        ],
    );
    log.append(call(Seq::new(6), FinishReason::Length)).unwrap();
    assert_eq!(
        summary(&check(&log.read_all().unwrap()).await, "generations"),
        "3 (1 idle, 1 incomplete)"
    );
}

#[tokio::test]
async fn tool_call_markup_and_unanswered_input_fail_the_run() {
    let log = memory_log();
    record(
        &log,
        vec![
            Step::Other(Event::Start),
            Step::Generate,
            Step::Eval("<invoke name=\"eval\">\n</invoke>\n```js\nreturn 1;\n```"),
            Step::Settle(Outcome::Throw, "SyntaxError: Unexpected token '<'"),
            Step::Generate,
            Step::Eval("await new Promise(r => setTimeout(r, 600000));"),
            Step::Other(Event::Stdin { text: "hello?".into() }),
        ],
    );
    let report = check(&log.read_all().unwrap()).await;
    let (parses, input) = (&report.checks[0], &report.checks[1]);
    assert!(!parses.pass);
    assert!(parses.detail.starts_with("1/2 malformed (50.0%"), "{}", parses.detail);
    assert!(
        parses.detail.contains("seq 3; 1 contain tool-call markup or fences"),
        "{}",
        parses.detail
    );
    assert!(!input.pass);
    assert!(input.detail.contains("seq 7: never seen"), "{}", input.detail);
}

#[test]
fn last_scopes_the_report_to_the_latest_start_and_failures_exit_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let log = Trajectory::new(SqliteStore::new(db::open(&path, Access::ReadWrite).unwrap(), TRAJECTORY_TABLE).unwrap())
        .unwrap();
    record(
        &log,
        vec![
            Step::Other(Event::Start),
            Step::Generate,
            Step::Eval("<oops>"),
            Step::Settle(Outcome::Throw, "SyntaxError"),
            Step::Other(Event::Start),
            Step::Generate,
            Step::Eval("return 2;"),
            Step::Settle(Outcome::Return, "2"),
        ],
    );
    drop(log);

    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_report"))
            .arg(&path)
            .args(args)
            .output()
            .unwrap();
        (output.status.code(), String::from_utf8(output.stdout).unwrap())
    };
    let (code, out) = run(&[]);
    assert_eq!(code, Some(1));
    assert!(out.contains("FAIL  model output parses"), "{out}");
    let (code, out) = run(&["--last", "--timeline"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("evaluations     1\n"), "{out}");
    // The timeline is the trajectory as the model sees it, calls included.
    assert!(
        out.contains(" generation]\n| anthropic/test through=5: 0.0s, stop"),
        "{out}"
    );
    assert_eq!(run(&["--bogus"]).0, Some(64));
}
