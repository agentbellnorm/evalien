use std::time::Duration;

use evalien::domain::{Event, Outcome, Seq};
use evalien::harness::policy::{IDLE_DELAYS, from_human, idle_delay, wakes};

fn every_event() -> Vec<Event> {
    vec![
        Event::Start,
        Event::Eval { code: "1".into() },
        Event::Stdin { text: "hi".into() },
        Event::Stdout { text: "out".into() },
        Event::Stderr { text: "err".into() },
        Event::Result {
            eval: Seq::new(1),
            outcome: Outcome::Return,
            text: "1".into(),
        },
    ]
}

#[test]
fn which_observations_wake_the_model_with_and_without_pending_evaluations() {
    let idle: Vec<bool> = every_event().iter().map(|e| wakes(e, 0)).collect();
    let busy: Vec<bool> = every_event().iter().map(|e| wakes(e, 1)).collect();
    assert_eq!(idle, [true, false, true, true, true, true]);
    assert_eq!(busy, [true, false, true, false, false, true]);
}

#[test]
fn only_a_fresh_start_or_human_input_resets_pacing() {
    let human: Vec<bool> = every_event().iter().map(from_human).collect();
    assert_eq!(human, [true, false, true, false, false, false]);
}

#[test]
fn quiet_calls_get_further_apart_capped_at_the_last_delay() {
    let delays: Vec<Duration> = (0..=9).map(|quiet| idle_delay(quiet, &IDLE_DELAYS)).collect();
    let secs: Vec<u64> = delays.iter().map(Duration::as_secs).collect();
    assert_eq!(secs, [0, 0, 0, 0, 5, 30, 120, 600, 600, 600]);
    assert_eq!(idle_delay(3, &[]), Duration::ZERO);
}
