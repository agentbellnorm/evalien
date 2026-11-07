//! The OpenAI Responses API with a strict JSON schema. Caching is automatic.

use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{FinishReason, GenerateError, Model, Prompt, Reply};

use super::{Answer, ModelConfig, endpoint, pricing::Tokens, send, settle, source_schema};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAi {
    config: ModelConfig,
    http: reqwest::Client,
}

impl OpenAi {
    pub fn new(config: ModelConfig, http: reqwest::Client) -> Self {
        OpenAi { config, http }
    }
}

impl Model for OpenAi {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let url = endpoint(self.config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL), "responses");
        let request = self
            .http
            .post(url)
            .bearer_auth(self.config.api_key.expose())
            .json(&request_body(&self.config, prompt));
        let started = Instant::now();
        let response: Response = send(request).await?;
        settle(&self.config, started.elapsed(), response.into())
    }
}

fn request_body(config: &ModelConfig, prompt: &Prompt) -> Value {
    let content: Vec<Value> = prompt
        .blocks
        .iter()
        .map(|text| json!({ "type": "input_text", "text": text }))
        .collect();
    json!({
        "model": config.model,
        "instructions": prompt.instructions,
        "input": [{ "role": "user", "content": content }],
        "max_output_tokens": config.max_output_tokens,
        "text": { "format": { "type": "json_schema", "name": "evaluation", "schema": source_schema(), "strict": true } },
        "store": false,
    })
}

#[derive(Deserialize)]
struct Response {
    status: Option<String>,
    incomplete_details: Option<Incomplete>,
    #[serde(default)]
    output: Vec<Item>,
    usage: Usage,
}

#[derive(Deserialize)]
struct Incomplete {
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Item {
    Message {
        content: Vec<Content>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Content {
    OutputText {
        text: String,
    },
    Refusal {},
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    input_tokens_details: Option<InputDetails>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Deserialize)]
struct InputDetails {
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct OutputDetails {
    reasoning_tokens: u64,
}

impl From<Response> for Answer {
    fn from(response: Response) -> Answer {
        let contents: Vec<Content> = response
            .output
            .into_iter()
            .flat_map(|item| match item {
                Item::Message { content } => content,
                Item::Other => Vec::new(),
            })
            .collect();
        let refused = contents.iter().any(|content| matches!(content, Content::Refusal {}));
        let finish_reason = match (response.status.as_deref(), response.incomplete_details) {
            (Some("completed"), _) if refused => FinishReason::ContentFilter,
            (Some("completed"), _) => FinishReason::Stop,
            (Some("incomplete"), Some(Incomplete { reason: Some(reason) })) => match reason.as_str() {
                "max_output_tokens" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                _ => FinishReason::Other,
            },
            (Some("failed"), _) => FinishReason::Error,
            (Some(_), _) => FinishReason::Other,
            (None, _) => FinishReason::Unknown,
        };
        let text = contents.into_iter().find_map(|content| match content {
            Content::OutputText { text } => Some(text),
            _ => None,
        });
        let usage = response.usage;
        let cached = usage.input_tokens_details.map_or(0, |details| details.cached_tokens);
        Answer {
            tokens: Tokens {
                uncached_input: usage.input_tokens.saturating_sub(cached),
                cache_read: cached,
                cache_write: 0,
                output: usage.output_tokens,
                reasoning: usage
                    .output_tokens_details
                    .map_or(0, |details| details.reasoning_tokens),
            },
            finish_reason,
            text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::{Provider, tests::config};

    #[test]
    fn requests_strict_source_with_instructions_and_ordered_blocks() {
        let prompt = Prompt {
            instructions: "be good".into(),
            blocks: vec!["a".into(), "b".into()],
        };
        let body = request_body(&config(Provider::OpenAi, "gpt-5.4"), &prompt);
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["instructions"], "be good");
        assert_eq!(body["input"][0]["content"][1]["text"], "b");
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(body["max_output_tokens"], 1000);
    }

    #[test]
    fn usage_and_status_map_to_tokens_and_finish_reasons() {
        let response: Response = serde_json::from_value(json!({
            "status": "completed",
            "output": [
                { "type": "reasoning", "summary": [] },
                { "type": "message", "content": [{ "type": "output_text", "text": "{\"code\":\"\"}" }] },
            ],
            "usage": {
                "input_tokens": 100, "input_tokens_details": { "cached_tokens": 60 },
                "output_tokens": 30, "output_tokens_details": { "reasoning_tokens": 20 },
            },
        }))
        .unwrap();
        let answer = Answer::from(response);
        assert_eq!(
            answer.tokens,
            Tokens {
                uncached_input: 40,
                cache_read: 60,
                cache_write: 0,
                output: 30,
                reasoning: 20
            }
        );
        assert_eq!(answer.finish_reason, FinishReason::Stop);
        assert_eq!(answer.text.as_deref(), Some("{\"code\":\"\"}"));

        let truncated: Response = serde_json::from_value(json!({
            "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" },
            "output": [], "usage": { "input_tokens": 1, "output_tokens": 1 },
        }))
        .unwrap();
        assert_eq!(Answer::from(truncated).finish_reason, FinishReason::Length);
    }
}
