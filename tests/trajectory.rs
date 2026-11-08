use std::{sync::Arc, time::Duration};

use evalien::db::{self, Access, MemoryStore, SqliteStore};
use evalien::domain::codec::{decode, encode};
use evalien::domain::{
    Entry, Event, FinishReason, Generation, Outcome, RecordStore, Seq, TRAJECTORY_TABLE, Timestamp, Trajectory,
    TrajectoryError, Usage, Usd,
};
use evalien::harness::context::{MAX_ENTRY_CHARS, MAX_PROMPT_CHARS, prompt_window, render_entry, render_trajectory};
use serde_json::json;

fn events() -> Vec<Event> {
    vec![
        Event::Start,
        Event::Eval {
            code: "const message = \"👽\";\nreturn message;".into(),
        },
        Event::Stdin {
            text: "  hello\n[result]\n\"quoted\"\u{0}  ".into(),
        },
        Event::Stdout { text: "👽\n".into() },
        Event::Stderr {
            text: "warning\n".into(),
        },
        Event::Result {
            eval: Seq::new(2),
            outcome: Outcome::Return,
            text: "undefined".into(),
        },
        Event::Result {
            eval: Seq::new(2),
            outcome: Outcome::Throw,
            text: "TypeError: broken\n  at repl:1".into(),
        },
        Event::Generation {
            through: Seq::new(7),
            generation: Generation {
                model: "anthropic/test".into(),
                latency: Duration::from_millis(2300),
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    uncached_input_tokens: 4,
                    cache_read_tokens: 3150,
                    cache_write_tokens: 531,
                    output_tokens: 213,
                    reasoning_tokens: 40,
                    cost: Usd::new(0.0041).unwrap(),
                },
            },
        },
        Event::Notice {
            text: "Model calls pause until midnight.".into(),
        },
    ]
}

fn entries() -> Vec<Entry> {
    let timestamp: Timestamp = "2026-09-20T12:00:00.000Z".parse().unwrap();
    events()
        .into_iter()
        .enumerate()
        .map(|(i, event)| Entry {
            seq: Seq::new(i as u64 + 1),
            timestamp,
            event,
        })
        .collect()
}

fn sqlite() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    (dir, path)
}

fn open(path: &std::path::Path) -> Trajectory {
    Trajectory::new(SqliteStore::new(db::open(path, Access::ReadWrite).unwrap(), TRAJECTORY_TABLE).unwrap()).unwrap()
}

#[test]
fn every_event_round_trips_without_losing_whitespace_unicode_or_outcome() {
    for event in events() {
        let encoded = encode(&event);
        assert_eq!(decode(&encoded).unwrap(), event);
        assert_eq!(encode(&decode(&encoded).unwrap()), encoded);
    }
}

#[test]
fn the_stored_format_is_canonical_versioned_json_compatible_with_the_typescript_runtime() {
    let result = Event::Result {
        eval: Seq::new(2),
        outcome: Outcome::Return,
        text: "3".into(),
    };
    assert_eq!(
        encode(&result),
        r#"{"version":1,"event":{"type":"result","evalId":2,"outcome":"return","text":"3"}}"#
    );
    assert_eq!(encode(&Event::Start), r#"{"version":1,"event":{"type":"start"}}"#);
    let generation = r#"{"version":1,"event":{"type":"generation","through":7,"model":"anthropic/test","latencyMs":2300,"finishReason":"stop","usage":{"uncachedInputTokens":4,"cacheReadTokens":3150,"cacheWriteTokens":531,"outputTokens":213,"reasoningTokens":40,"costUSD":0.0041}}}"#;
    assert_eq!(encode(&decode(generation).unwrap()), generation);
}

#[test]
fn decoding_rejects_corrupt_unknown_and_incomplete_records() {
    let usage = json!({ "uncachedInputTokens": 0, "cacheReadTokens": 0, "cacheWriteTokens": 0, "outputTokens": 0, "reasoningTokens": 0, "costUSD": 0 });
    let with_usage = |field: &str, value: serde_json::Value| {
        let mut usage = usage.clone();
        usage[field] = value;
        json!({ "type": "generation", "through": 1, "model": "m", "latencyMs": 1, "finishReason": "stop", "usage": usage })
    };
    let mut invalid = vec![
        json!(null),
        json!([]),
        json!({}),
        json!({ "type": "start" }),
        json!({ "version": 2, "event": { "type": "start" } }),
        json!({ "version": 1, "event": { "type": "start" }, "ignored": true }),
    ];
    let bad_events = [
        json!(null),
        json!([]),
        json!({}),
        json!({ "type": "wake" }),
        json!({ "type": "start", "extra": true }),
        json!({ "type": "eval" }),
        json!({ "type": "eval", "code": 42 }),
        json!({ "type": "stdout", "text": null }),
        json!({ "type": "result", "evalId": 2, "outcome": "return" }),
        json!({ "type": "result", "evalId": 2, "outcome": "timeout", "text": "x" }),
        json!({ "type": "generation", "through": 1, "model": "m", "latencyMs": 1, "finishReason": "stop" }),
        json!({ "type": "generation", "through": 1, "model": "m", "latencyMs": 1, "finishReason": "done", "usage": usage }),
        with_usage("costUSD", json!(-1)),
        with_usage("costUSD", json!(null)),
        with_usage("outputTokens", json!(1.5)),
        with_usage("cacheReadTokens", json!("3")),
        json!({ "type": "result", "evalId": 0, "outcome": "return", "text": "x" }),
        json!({ "type": "result", "evalId": -1, "outcome": "return", "text": "x" }),
        json!({ "type": "result", "evalId": 1.5, "outcome": "return", "text": "x" }),
        json!({ "type": "result", "evalId": "2", "outcome": "return", "text": "x" }),
    ];
    invalid.extend(
        bad_events
            .into_iter()
            .map(|event| json!({ "version": 1, "event": event })),
    );
    for value in invalid {
        assert!(decode(&value.to_string()).is_err(), "accepted {value}");
    }
    assert!(decode(r#"{"version":1"#).is_err());
}

#[test]
fn rendering_shows_identity_completion_and_framed_multiline_content() {
    let entries = entries();
    assert_eq!(
        render_entry(&entries[5]),
        "[6 2026-09-20T12:00:00.000Z result eval=2 return]\n| undefined\n\n"
    );
    assert_eq!(
        render_entry(&entries[2]),
        "[3 2026-09-20T12:00:00.000Z stdin]\n|   hello\n| [result]\n| \"quoted\"\u{0}  \n\n"
    );
    assert!(render_entry(&entries[0]).contains("Previous live bindings"));
    assert!(render_entry(&entries[6]).contains("result eval=2 throw"));
    assert_eq!(
        render_entry(&entries[7]),
        "[8 2026-09-20T12:00:00.000Z generation]\n\
         | anthropic/test through=7: 2.3s, stop, in 4 + 3150 cached + 531 written, out 213 (40 reasoning), $0.0041\n\n"
    );
}

#[test]
fn every_appended_suffix_preserves_the_rendered_prefix() {
    let entries = entries();
    let whole = render_trajectory(&entries);
    for i in 0..=entries.len() {
        assert_eq!(
            render_trajectory(&entries[..i]) + &render_trajectory(&entries[i..]),
            whole
        );
    }
}

#[test]
fn oversized_entries_keep_their_head_and_tail_and_point_at_the_full_event() {
    let text = "a".repeat(MAX_ENTRY_CHARS) + "MIDDLE" + &"z".repeat(MAX_ENTRY_CHARS);
    let entry = Entry {
        seq: Seq::new(7),
        timestamp: Timestamp::now(),
        event: Event::Stdout { text },
    };
    let rendered = render_entry(&entry);
    assert!(rendered.len() < MAX_ENTRY_CHARS + 500);
    assert!(rendered.contains(" stdout]\n| a"));
    assert!(rendered.ends_with("z\n\n"));
    assert!(rendered.contains("8006 characters omitted; full text in trajectory seq 7"));
    assert!(!rendered.contains("MIDDLE"));
}

#[test]
fn the_prompt_window_stays_aligned_when_small_and_bounded_when_output_floods() {
    let log = Trajectory::new(MemoryStore::new()).unwrap();
    for i in 0..450 {
        log.append(Event::Stdout {
            text: format!("line {i}\n"),
        })
        .unwrap();
    }
    assert_eq!(prompt_window(&log, Seq::new(450)).unwrap().len(), 450);

    for _ in 0..150 {
        log.append(Event::Stdout {
            text: "x".repeat(MAX_ENTRY_CHARS),
        })
        .unwrap();
    }
    let through = log.last_seq();
    let blocks = prompt_window(&log, through).unwrap();
    assert!(blocks.iter().map(|b| b.chars().count()).sum::<usize>() <= MAX_PROMPT_CHARS);
    let last = log.read(Seq::new(through.get() - 1), through).unwrap();
    assert_eq!(blocks.last(), Some(&render_entry(&last[0])));
}

#[test]
fn sqlite_persists_an_ordered_trajectory_and_reads_fixed_ranges() {
    let (_dir, path) = sqlite();
    let log = open(&path);
    for event in events().into_iter().take(5) {
        log.append(event).unwrap();
    }
    let result = Event::Result {
        eval: Seq::new(2),
        outcome: Outcome::Return,
        text: "undefined".into(),
    };
    log.append(result).unwrap();
    drop(log);

    let reopened = open(&path);
    assert_eq!(reopened.last_seq(), Seq::new(6));
    let all = reopened.read_all().unwrap();
    assert_eq!(
        all.iter().map(|e| e.event.clone()).collect::<Vec<_>>()[..5],
        events()[..5]
    );
    let range = reopened.read(Seq::new(2), Seq::new(4)).unwrap();
    assert_eq!(range.iter().map(|e| e.seq.get()).collect::<Vec<_>>(), [3, 4]);

    let read_only = SqliteStore::new(db::open(&path, Access::ReadOnly).unwrap(), TRAJECTORY_TABLE).unwrap();
    assert_eq!(read_only.last_seq().unwrap(), Seq::new(6));
}

#[test]
fn records_written_by_the_typescript_runtime_are_readable() {
    let (_dir, path) = sqlite();
    drop(open(&path));
    let connection = db::open(&path, Access::ReadWrite).unwrap();
    connection
        .execute(
            "INSERT INTO trajectory (timestamp, data) VALUES ('2026-09-20T12:00:00.000Z', ?1)",
            [r#"{"version":1,"event":{"type":"stdin","text":"hi"}}"#],
        )
        .unwrap();
    let entries = open(&path).read_all().unwrap();
    assert_eq!(entries[0].event, Event::Stdin { text: "hi".into() });
    assert_eq!(entries[0].timestamp.to_string(), "2026-09-20T12:00:00.000Z");
}

#[test]
fn corrupt_stored_events_fail_visibly_instead_of_silently_changing_history() {
    let (_dir, path) = sqlite();
    drop(open(&path));
    let connection = db::open(&path, Access::ReadWrite).unwrap();
    connection
        .execute(
            "INSERT INTO trajectory (timestamp, data) VALUES ('2026-09-20T12:00:00.000Z', '{\"version\":1}')",
            [],
        )
        .unwrap();
    assert!(matches!(open(&path).read_all(), Err(TrajectoryError::Corrupt { seq, .. }) if seq == Seq::new(1)));
}

#[tokio::test]
async fn dangling_results_cannot_be_appended_and_do_not_wake_consumers() {
    let log = Arc::new(Trajectory::new(MemoryStore::new()).unwrap());
    log.append(Event::Stdout {
        text: "not an eval".into(),
    })
    .unwrap();
    for eval in [Seq::new(1), Seq::new(9)] {
        let dangling = Event::Result {
            eval,
            outcome: Outcome::Return,
            text: "x".into(),
        };
        assert!(matches!(log.append(dangling), Err(TrajectoryError::DanglingResult(_))));
    }
    assert_eq!(log.last_seq(), Seq::new(1));
    let waiting = tokio::time::timeout(Duration::from_millis(20), log.wait_after(Seq::new(1))).await;
    assert!(waiting.is_err(), "a rejected append woke a consumer");
}

#[tokio::test]
async fn consumers_have_independent_cursors_and_wake_only_beyond_their_sequence() {
    let log = Arc::new(Trajectory::new(MemoryStore::new()).unwrap());
    log.append(Event::Start).unwrap();
    // Already beyond: returns at once.
    assert_eq!(log.wait_after(Seq::ZERO).await, Seq::new(1));

    let waiter = tokio::spawn({
        let log = log.clone();
        async move { log.wait_after(Seq::new(1)).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiter.is_finished());
    log.append(Event::Stdin { text: "a".into() }).unwrap();
    log.append(Event::Stdin { text: "b".into() }).unwrap();
    // A burst is seen whole: the waiter returns the latest sequence it saw.
    let seen = waiter.await.unwrap();
    assert!(seen >= Seq::new(2));
    assert_eq!(log.wait_after(Seq::new(2)).await, Seq::new(3));
}

#[test]
fn the_memory_store_behaves_like_sqlite_behind_the_trajectory() {
    let (_dir, path) = sqlite();
    let stores = [Trajectory::new(MemoryStore::new()).unwrap(), open(&path)];
    for log in stores {
        for event in events().into_iter().take(5) {
            log.append(event).unwrap();
        }
        assert_eq!(log.read(Seq::new(1), Seq::new(3)).unwrap().len(), 2);
        assert_eq!(log.read(Seq::new(3), Seq::new(3)).unwrap().len(), 0);
        assert!(matches!(
            log.append(Event::Result {
                eval: Seq::new(3),
                outcome: Outcome::Return,
                text: "x".into()
            }),
            Err(TrajectoryError::DanglingResult(_))
        ));
    }
}
