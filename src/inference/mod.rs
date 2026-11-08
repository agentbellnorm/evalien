//! Model calls over each provider's HTTP API. Provider formats, credentials
//! and cache state stay behind [`Model`].
//!
//! Every provider follows the same shape: build a request, parse the response
//! into an [`Answer`], then [`settle`] it into a reply or a billed failure.

mod anthropic;
mod budget;
pub mod config;
mod google;
mod openai;
pub mod pricing;

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{FinishReason, GenerateError, Generation, Model, Prompt, Reply};

pub use anthropic::Anthropic;
pub use budget::{Budgeted, spent_on_day};
pub use config::{ConfigError, InferenceConfig, ModelConfig, Provider};
pub use google::Google;
pub use openai::OpenAi;
use pricing::Tokens;

/// The configured provider.
pub enum AnyModel {
    Anthropic(Anthropic),
    OpenAi(OpenAi),
    Google(Google),
}

pub fn connect(config: ModelConfig) -> Result<AnyModel, ConfigError> {
    let http = reqwest::Client::builder()
        .user_agent("evalien")
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| ConfigError(format!("HTTP client: {e}")))?;
    Ok(match config.provider {
        Provider::Anthropic => AnyModel::Anthropic(Anthropic::new(config, http)),
        Provider::OpenAi => AnyModel::OpenAi(OpenAi::new(config, http)),
        Provider::Google => AnyModel::Google(Google::new(config, http)),
    })
}

impl Model for AnyModel {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        match self {
            AnyModel::Anthropic(model) => model.generate(prompt).await,
            AnyModel::OpenAi(model) => model.generate(prompt).await,
            AnyModel::Google(model) => model.generate(prompt).await,
        }
    }
}

/// The reply as a provider reported it, before deciding whether it's usable.
#[derive(Clone, Debug, PartialEq)]
struct Answer {
    tokens: Tokens,
    finish_reason: FinishReason,
    text: Option<String>,
}

/// The reply object every provider is constrained to produce.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    code: String,
}

fn source_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "JavaScript source for an async function body, or empty to do nothing until something happens",
            },
        },
        "required": ["code"],
        "additionalProperties": false,
    })
}

/// Price the answer and accept it only if it completed with the source object.
/// Rejected answers are still billed, so they carry their generation.
fn settle(config: &ModelConfig, latency: Duration, answer: Answer) -> Result<Reply, GenerateError> {
    let generation = Generation {
        model: config.id(),
        latency,
        finish_reason: answer.finish_reason,
        usage: config.pricing.usage(answer.tokens),
    };
    if answer.finish_reason != FinishReason::Stop {
        let reason = format!("Generation did not complete: {}", answer.finish_reason.as_str());
        return Err(GenerateError::Unusable { generation, reason });
    }
    let parsed = answer
        .text
        .ok_or_else(|| "no text in the reply".to_owned())
        .and_then(|text| serde_json::from_str::<Source>(&text).map_err(|e| e.to_string()));
    match parsed {
        Ok(source) => Ok(Reply {
            generation,
            code: source.code,
        }),
        Err(reason) => Err(GenerateError::Unusable {
            generation,
            reason: format!("Generation did not produce source: {reason}"),
        }),
    }
}

/// Send a request and decode a successful response. Anything else failed
/// before a billed reply existed.
async fn send<T: serde::de::DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T, GenerateError> {
    let response = request.send().await.map_err(|e| GenerateError::Failed(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| GenerateError::Failed(e.to_string()))?;
    if !status.is_success() {
        let excerpt: String = body.chars().take(500).collect();
        return Err(GenerateError::Failed(format!("{status}: {excerpt}")));
    }
    serde_json::from_str(&body).map_err(|e| GenerateError::Failed(format!("unexpected response: {e}")))
}

/// Join a base URL and a path without doubling or dropping the slash.
fn endpoint(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::pricing::Pricing;

    pub fn config(provider: Provider, model: &str) -> ModelConfig {
        ModelConfig {
            provider,
            model: model.into(),
            api_key: crate::domain::Secret::new("test-key"),
            base_url: None,
            max_output_tokens: 1000,
            pricing: Pricing {
                input: 2.0,
                output: 10.0,
                cache_read: 0.2,
                cache_write: 2.5,
            },
        }
    }

    fn answer(finish_reason: FinishReason, text: &str) -> Answer {
        Answer {
            tokens: Tokens {
                output: 100_000,
                ..Tokens::default()
            },
            finish_reason,
            text: Some(text.into()),
        }
    }

    #[test]
    fn only_a_completed_source_object_is_accepted_but_rejected_calls_are_still_billed() {
        let config = config(Provider::Anthropic, "m");
        let latency = Duration::from_millis(5);
        let reply = settle(&config, latency, answer(FinishReason::Stop, r#"{"code":"return 1"}"#)).unwrap();
        assert_eq!(reply.code, "return 1");
        assert_eq!(reply.generation.usage.cost.get(), 1.0);

        for (finish, text, expected) in [
            (
                FinishReason::Length,
                r#"{"code":"retu"#,
                "Generation did not complete: length",
            ),
            (
                FinishReason::Stop,
                "```js\nreturn 1\n```",
                "Generation did not produce source",
            ),
            (
                FinishReason::Stop,
                r#"{"code":"x","extra":1}"#,
                "Generation did not produce source",
            ),
        ] {
            match settle(&config, latency, answer(finish, text)) {
                Err(GenerateError::Unusable { generation, reason }) => {
                    assert!(reason.starts_with(expected), "{reason}");
                    assert_eq!(generation.usage.cost.get(), 1.0);
                }
                other => panic!("expected a billed rejection, got {other:?}"),
            }
        }
    }
}
