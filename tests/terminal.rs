use std::time::Duration;

use evalien::db::MemoryStore;
use evalien::domain::{Entry, Event, FinishReason, Generation, Outcome, Seq, Timestamp, Trajectory, Usage};
use evalien::terminal::{
    display::{Line, line},
    input::forward_lines,
};

fn entry(seq: u64, event: Event) -> Entry {
    Entry {
        seq: Seq::new(seq),
        timestamp: Timestamp::now(),
        event,
    }
}

#[test]
fn display_prints_input_source_results_output_and_calls() {
    let shown = |event| line(&entry(3, event));
    assert!(matches!(shown(Event::Start), Some(Line::Out(t)) if t.contains("2 prior events")));
    assert!(matches!(shown(Event::Stdin { text: "hi".into() }), Some(Line::Out(t)) if t.contains("[you] hi")));
    assert!(matches!(shown(Event::Eval { code: "a\nb".into() }), Some(Line::Out(t)) if t.contains("a\n  b")));
    assert_eq!(
        shown(Event::Stdout { text: "out".into() }),
        Some(Line::Out("out".into()))
    );
    assert_eq!(
        shown(Event::Stderr { text: "err".into() }),
        Some(Line::Err("err".into()))
    );
    let result = |outcome, text: &str| Event::Result {
        eval: Seq::new(1),
        outcome,
        text: text.into(),
    };
    assert!(matches!(shown(result(Outcome::Throw, "boom")), Some(Line::Err(t)) if t.contains("boom")));
    assert!(matches!(shown(result(Outcome::Return, "42")), Some(Line::Out(t)) if t.contains("42")));
    assert_eq!(shown(result(Outcome::Return, "undefined")), None);
    let generation = Generation {
        model: "test/model".into(),
        latency: Duration::from_millis(1200),
        finish_reason: FinishReason::Stop,
        usage: Usage {
            uncached_input_tokens: 1,
            cache_read_tokens: 2,
            cache_write_tokens: 3,
            ..Usage::default()
        },
    };
    let note = shown(Event::Generation {
        through: Seq::new(2),
        generation,
    });
    assert!(matches!(note, Some(Line::Note(t)) if t.starts_with("test/model 1200ms stop | 6 in, 2 cached")));
}

#[tokio::test]
async fn input_forwards_nonblank_lines_until_the_stream_ends() {
    let log = Trajectory::new(MemoryStore::new()).unwrap();
    forward_lines(&b"hello\n\n   \nthere\n"[..], &log).await.unwrap();
    let typed: Vec<Event> = log.read_all().unwrap().into_iter().map(|e| e.event).collect();
    assert_eq!(
        typed,
        [
            Event::Stdin { text: "hello".into() },
            Event::Stdin { text: "there".into() }
        ]
    );
}
