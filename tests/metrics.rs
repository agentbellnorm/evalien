use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use evalien::domain::{Entry, Event, FinishReason, Generation, Outcome, Seq, Timestamp, Usage, Usd};
use evalien::metrics::{Metrics, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Noon UTC on 2026-11-07.
fn noon() -> Timestamp {
    Timestamp::from_system_time(SystemTime::UNIX_EPOCH + Duration::from_secs(20_399 * 86_400 + 12 * 3600))
}

fn at(seconds: u64) -> Timestamp {
    Timestamp::from_system_time(noon().as_system_time() + Duration::from_secs(seconds))
}

fn entry(seq: u64, timestamp: Timestamp, event: Event) -> Entry {
    Entry {
        seq: Seq::new(seq),
        timestamp,
        event,
    }
}

fn call(model: &str, finish_reason: FinishReason, cost: f64, latency_ms: u64) -> Event {
    Event::Generation {
        through: Seq::ZERO,
        generation: Generation {
            model: model.into(),
            latency: Duration::from_millis(latency_ms),
            finish_reason,
            usage: Usage {
                uncached_input_tokens: 100,
                cache_read_tokens: 900,
                cache_write_tokens: 50,
                output_tokens: 40,
                reasoning_tokens: 10,
                cost: Usd::new(cost).unwrap(),
            },
        },
    }
}

fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line {prefix} in:\n{text}"))
}

#[test]
fn model_calls_tokens_cost_latency_and_evaluations_are_counted() {
    let metrics = Metrics::new(Usd::new(2.0).unwrap(), Usd::new(0.5).unwrap(), noon());
    for e in [
        entry(1, at(0), Event::Start),
        entry(
            2,
            at(1),
            call("anthropic/claude-sonnet-5-5", FinishReason::Stop, 0.01, 2_300),
        ),
        entry(
            3,
            at(2),
            Event::Eval {
                code: "await sleep()".into(),
            },
        ),
        entry(
            4,
            at(3),
            call("anthropic/claude-sonnet-5-5", FinishReason::Length, 0.02, 700),
        ),
        entry(
            5,
            at(12),
            Event::Result {
                eval: Seq::new(3),
                outcome: Outcome::Return,
                text: "1".into(),
            },
        ),
        entry(
            6,
            at(13),
            Event::Eval {
                code: "await forever()".into(),
            },
        ),
    ] {
        metrics.observe(&e);
    }
    let text = metrics.render(at(14));
    let sonnet = r#"provider="anthropic",model="claude-sonnet-5-5""#;
    assert_eq!(
        line(
            &text,
            &format!("evalien_model_calls_total{{{sonnet},finish_reason=\"stop\"}}")
        ),
        format!("evalien_model_calls_total{{{sonnet},finish_reason=\"stop\"}} 1")
    );
    assert!(
        line(
            &text,
            &format!("evalien_model_calls_total{{{sonnet},finish_reason=\"length\"}}")
        )
        .ends_with(" 1")
    );
    assert!(
        line(
            &text,
            &format!("evalien_model_tokens_total{{{sonnet},class=\"input_cache_read\"}}")
        )
        .ends_with(" 1800")
    );
    assert!(
        line(
            &text,
            &format!("evalien_model_tokens_total{{{sonnet},class=\"output_reasoning\"}}")
        )
        .ends_with(" 20")
    );
    assert!(line(&text, &format!("evalien_model_cost_usd_total{{{sonnet}}}")).ends_with(" 0.03"));
    assert!(
        line(
            &text,
            &format!("evalien_model_call_duration_seconds_bucket{{{sonnet},le=\"1\"}}")
        )
        .ends_with(" 1")
    );
    assert!(
        line(
            &text,
            &format!("evalien_model_call_duration_seconds_bucket{{{sonnet},le=\"3\"}}")
        )
        .ends_with(" 2")
    );
    assert!(line(&text, &format!("evalien_model_call_duration_seconds_sum{{{sonnet}}}")).ends_with(" 3"));
    assert_eq!(line(&text, "evalien_budget_daily_usd "), "evalien_budget_daily_usd 2");
    assert_eq!(
        line(&text, "evalien_budget_spent_today_usd "),
        "evalien_budget_spent_today_usd 0.53"
    );
    assert!(line(&text, r#"evalien_events_total{type="eval"}"#).ends_with(" 2"));
    assert!(
        line(
            &text,
            r#"evalien_evaluation_duration_seconds_bucket{outcome="return",le="5"}"#
        )
        .ends_with(" 0")
    );
    assert!(
        line(
            &text,
            r#"evalien_evaluation_duration_seconds_bucket{outcome="return",le="30"}"#
        )
        .ends_with(" 1")
    );
    assert_eq!(
        line(&text, "evalien_evaluations_pending "),
        "evalien_evaluations_pending 1"
    );
    assert!(text.contains("# TYPE evalien_model_call_duration_seconds histogram"));
}

#[test]
fn todays_spend_starts_over_at_midnight_utc() {
    let metrics = Metrics::new(Usd::new(2.0).unwrap(), Usd::new(0.5).unwrap(), noon());
    let tomorrow = noon().next_utc_midnight();
    assert_eq!(
        line(&metrics.render(tomorrow), "evalien_budget_spent_today_usd "),
        "evalien_budget_spent_today_usd 0"
    );
    metrics.observe(&entry(
        1,
        tomorrow,
        call("openai/gpt-5.4", FinishReason::Stop, 0.25, 500),
    ));
    assert_eq!(
        line(&metrics.render(tomorrow), "evalien_budget_spent_today_usd "),
        "evalien_budget_spent_today_usd 0.25"
    );
}

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: test\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn the_endpoint_serves_metrics_to_prometheus_and_nothing_else() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics = Arc::new(Metrics::new(Usd::new(1.0).unwrap(), Usd::ZERO, Timestamp::now()));
    tokio::spawn(serve(listener, metrics, |_| {}));

    let scrape = get(addr, "/metrics").await;
    assert!(scrape.starts_with("HTTP/1.1 200 OK"), "{scrape}");
    assert!(scrape.contains("content-type: text/plain; version=0.0.4"));
    assert!(scrape.contains("evalien_budget_daily_usd 1"));
    assert!(get(addr, "/").await.starts_with("HTTP/1.1 404"));
}
