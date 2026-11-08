use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use evalien::domain::{
    Entry, Event, FinishReason, GenerateError, Generation, Model, Prompt, Reply, Seq, Timestamp, Usage, Usd,
};
use evalien::inference::{
    Budgeted, Provider, config,
    pricing::{Pricing, Tokens},
    spent_on_day,
};

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn cost_prices_each_token_class_at_its_own_rate() {
    let pricing = Pricing {
        input: 2.0,
        output: 10.0,
        cache_read: 0.2,
        cache_write: 2.5,
    };
    let tokens = Tokens {
        uncached_input: 1_000_000,
        cache_read: 1_000_000,
        cache_write: 1_000_000,
        output: 1_000_000,
        reasoning: 7,
    };
    let usage = pricing.usage(tokens);
    assert!((usage.cost.get() - 14.7).abs() < 1e-9);
    assert_eq!(usage.reasoning_tokens, 7);
}

#[test]
fn model_configuration_validates_selection_and_only_requires_the_selected_providers_key() {
    let selected = config::read(&env(&[("ANTHROPIC_API_KEY", " key ")])).unwrap();
    assert_eq!(selected.model.provider, Provider::Anthropic);
    assert_eq!(selected.model.id(), "anthropic/claude-sonnet-5-5");
    assert_eq!(selected.model.api_key.expose(), "key");
    assert_eq!(format!("{:?}", selected.model.api_key), "Secret(…)");
    assert_eq!(selected.budget, Usd::new(1.0).unwrap());

    let openai = config::read(&env(&[
        ("MODEL", "openai/gpt-5.4"),
        ("OPENAI_API_KEY", "k"),
        ("MODEL_PRICING", "1,2"),
    ]))
    .unwrap();
    assert_eq!(
        openai.model.pricing,
        Pricing {
            input: 1.0,
            output: 2.0,
            cache_read: 1.0,
            cache_write: 1.0
        }
    );

    for (pairs, message) in [
        (
            vec![("MODEL", "mystery/x"), ("ANTHROPIC_API_KEY", "k")],
            "MODEL must be",
        ),
        (
            vec![("MODEL", "anthropic/ "), ("ANTHROPIC_API_KEY", "k")],
            "MODEL must be",
        ),
        (
            vec![("MODEL", "openai/gpt-5.4"), ("ANTHROPIC_API_KEY", "k")],
            "Set OPENAI_API_KEY",
        ),
        (
            vec![("MODEL", "openai/gpt-5.4"), ("OPENAI_API_KEY", "k")],
            "Set MODEL_PRICING",
        ),
        (
            vec![("ANTHROPIC_API_KEY", "k"), ("MODEL_MAX_OUTPUT_TOKENS", "0")],
            "MODEL_MAX_OUTPUT_TOKENS",
        ),
        (
            vec![("ANTHROPIC_API_KEY", "k"), ("MODEL_BUDGET_USD", "-1")],
            "MODEL_BUDGET_USD",
        ),
        (
            vec![("ANTHROPIC_API_KEY", "k"), ("MODEL_PRICING", "1,2,3")],
            "MODEL_PRICING must be",
        ),
    ] {
        let error = config::read(&env(&pairs)).unwrap_err().to_string();
        assert!(error.contains(message), "{pairs:?}: {error}");
    }
}

/// Replies with a fixed cost, or a billed failure for prompts saying "fail".
struct Fixed;

impl Model for Fixed {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let generation = Generation {
            model: "test/model".into(),
            latency: Duration::ZERO,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                cost: Usd::new(0.016).unwrap(),
                ..Usage::default()
            },
        };
        if prompt.instructions == "fail" {
            return Err(GenerateError::Unusable {
                generation,
                reason: "no source".into(),
            });
        }
        Ok(Reply {
            generation,
            code: String::new(),
        })
    }
}

/// Noon UTC on 2026-11-07, or midnight after it, depending on the test's clock.
fn noon() -> Timestamp {
    Timestamp::from_system_time(SystemTime::UNIX_EPOCH + Duration::from_secs(20_399 * 86_400 + 12 * 3600))
}

fn next_day() -> Timestamp {
    noon().next_utc_midnight()
}

#[tokio::test]
async fn the_daily_budget_counts_completed_and_failed_calls_and_earlier_runs() {
    // Earlier runs today spent a cent.
    let model = Budgeted::with_clock(Fixed, Usd::new(0.04).unwrap(), Usd::new(0.01).unwrap(), noon);
    let ok = Prompt {
        instructions: "ok".into(),
        blocks: vec![],
    };
    let fail = Prompt {
        instructions: "fail".into(),
        blocks: vec![],
    };
    model.generate(&ok).await.unwrap();
    assert!(matches!(
        model.generate(&fail).await,
        Err(GenerateError::Unusable { .. })
    ));
    // The call that crossed the limit completed; the next one is refused until midnight.
    match model.generate(&ok).await {
        Err(GenerateError::BudgetSpent { resumes_at, .. }) => assert_eq!(resumes_at, next_day()),
        other => panic!("expected a spent budget, got {other:?}"),
    }
    assert_eq!(model.spent_today().to_string(), "$0.0420");
}

#[tokio::test]
async fn the_budget_starts_over_at_midnight_utc() {
    let model = Budgeted::with_clock(Fixed, Usd::new(0.01).unwrap(), Usd::new(0.5).unwrap(), noon);
    let ok = Prompt {
        instructions: "ok".into(),
        blocks: vec![],
    };
    assert!(matches!(
        model.generate(&ok).await,
        Err(GenerateError::BudgetSpent { .. })
    ));
    let tomorrow = Budgeted::with_clock(Fixed, Usd::new(0.01).unwrap(), Usd::ZERO, next_day);
    tomorrow.generate(&ok).await.unwrap();
}

#[test]
fn todays_spend_comes_from_the_recorded_calls_of_that_day() {
    let call = |timestamp: Timestamp, cost: f64| Entry {
        seq: Seq::new(1),
        timestamp,
        event: Event::Generation {
            through: Seq::ZERO,
            generation: Generation {
                model: "m".into(),
                latency: Duration::ZERO,
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    cost: Usd::new(cost).unwrap(),
                    ..Usage::default()
                },
            },
        },
    };
    let yesterday = Timestamp::from_system_time(noon().as_system_time() - Duration::from_secs(86_400));
    let entries = [call(yesterday, 5.0), call(noon(), 0.25), call(noon(), 0.5)];
    assert_eq!(spent_on_day(&entries, noon()).to_string(), "$0.7500");
}
