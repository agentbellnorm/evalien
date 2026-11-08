//! The Anthropic Messages API, with structured output and prompt caching.

use std::{
    sync::{Mutex, PoisonError},
    time::Instant,
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{FinishReason, GenerateError, Model, Prompt, Reply};

use super::{Answer, ModelConfig, endpoint, pricing::Tokens, send, settle, source_schema};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const API_VERSION: &str = "2026-10-21";

pub struct Anthropic {
    config: ModelConfig,
    http: reqwest::Client,
    /// The last prompt that produced a reply, to place cache breakpoints.
    previous: Mutex<Option<Prompt>>,
}

impl Anthropic {
    pub fn new(config: ModelConfig, http: reqwest::Client) -> Self {
        Anthropic {
            config,
            http,
            previous: Mutex::new(None),
        }
    }

    fn previous(&self) -> std::sync::MutexGuard<'_, Option<Prompt>> {
        self.previous.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Model for Anthropic {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let reused = reused_prefix(self.previous().as_ref(), prompt);
        let url = endpoint(self.config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL), "messages");
        let request = self
            .http
            .post(url)
            .header("x-api-key", self.config.api_key.expose())
            .header("anthropic-version", API_VERSION)
            .json(&request_body(&self.config, prompt, reused));
        let started = Instant::now();
        let response: Response = send(request).await?;
        let reply = settle(&self.config, started.elapsed(), response.into())?;
        // Only a successful call moves the cached prefix forward.
        *self.previous() = Some(prompt.clone());
        Ok(reply)
    }
}

/// The last block of the previous prompt, if this prompt extends it. Marking
/// it lets the new prompt read the prefix the previous call wrote.
fn reused_prefix(previous: Option<&Prompt>, prompt: &Prompt) -> Option<usize> {
    let previous = previous?;
    let extends = previous.instructions == prompt.instructions
        && previous.blocks.len() <= prompt.blocks.len()
        && previous.blocks.iter().zip(&prompt.blocks).all(|(a, b)| a == b);
    (extends && !previous.blocks.is_empty()).then(|| previous.blocks.len() - 1)
}

fn request_body(config: &ModelConfig, prompt: &Prompt, reused: Option<usize>) -> Value {
    let cache = json!({ "type": "ephemeral" });
    let last = prompt.blocks.len().saturating_sub(1);
    let content: Vec<Value> = prompt
        .blocks
        .iter()
        .enumerate()
        .map(|(i, text)| {
            if i == last || Some(i) == reused {
                json!({ "type": "text", "text": text, "cache_control": cache })
            } else {
                json!({ "type": "text", "text": text })
            }
        })
        .collect();
    json!({
        "model": config.model,
        "max_tokens": config.max_output_tokens,
        "system": [{ "type": "text", "text": prompt.instructions, "cache_control": cache }],
        "messages": [{ "role": "user", "content": content }],
        // Constrained decoding: the reply is this object, never prose or fences.
        "output_config": { "format": { "type": "json_schema", "schema": source_schema() } },
    })
}

#[derive(Deserialize)]
struct Response {
    content: Vec<Block>,
    stop_reason: Option<String>,
    usage: Usage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl From<Response> for Answer {
    fn from(response: Response) -> Answer {
        let finish_reason = match response.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence") => FinishReason::Stop,
            Some("max_tokens") => FinishReason::Length,
            Some("refusal") => FinishReason::ContentFilter,
            Some("tool_use") => FinishReason::ToolCalls,
            Some(_) => FinishReason::Other,
            None => FinishReason::Unknown,
        };
        let text = response.content.into_iter().find_map(|block| match block {
            Block::Text { text } => Some(text),
            Block::Other => None,
        });
        let usage = response.usage;
        Answer {
            tokens: Tokens {
                uncached_input: usage.input_tokens,
                cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
                output: usage.output_tokens,
                reasoning: 0,
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

    fn prompt(blocks: &[&str]) -> Prompt {
        Prompt {
            instructions: "be good".into(),
            blocks: blocks.iter().map(|b| b.to_string()).collect(),
        }
    }

    fn cached(body: &Value) -> Vec<bool> {
        body["messages"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.get("cache_control").is_some())
            .collect()
    }

    #[test]
    fn requests_constrained_source_with_cached_instructions_and_ordered_blocks() {
        let body = request_body(
            &config(Provider::Anthropic, "claude-sonnet-5-5"),
            &prompt(&["a", "b"]),
            None,
        );
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(body["max_tokens"], 1000);
        assert_eq!(body["system"][0]["text"], "be good");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["required"][0], "code");
        assert_eq!(body["messages"][0]["content"][1]["text"], "b");
        assert_eq!(cached(&body), [false, true]);
        assert!(
            body.get("temperature").is_none(),
            "sampling parameters are rejected on this API version"
        );
    }

    #[test]
    fn a_prompt_extending_the_previous_one_reads_its_prefix() {
        let first = prompt(&["a", "b"]);
        let longer = prompt(&["a", "b", "c", "d"]);
        let changed = prompt(&["x", "b", "c"]);
        let config = config(Provider::Anthropic, "m");
        assert_eq!(
            cached(&request_body(&config, &longer, reused_prefix(Some(&first), &longer))),
            [false, true, false, true]
        );
        assert_eq!(
            cached(&request_body(&config, &changed, reused_prefix(Some(&first), &changed))),
            [false, false, true]
        );
        assert_eq!(reused_prefix(None, &first), None);
    }

    #[test]
    fn usage_separates_uncached_cached_and_output_tokens() {
        let response: Response = serde_json::from_value(json!({
            "content": [{ "type": "thinking", "thinking": "" }, { "type": "text", "text": "{\"code\":\"\"}" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 300, "cache_creation_input_tokens": 20 },
        }))
        .unwrap();
        let answer = Answer::from(response);
        assert_eq!(
            answer.tokens,
            Tokens {
                uncached_input: 10,
                cache_read: 300,
                cache_write: 20,
                output: 5,
                reasoning: 0
            }
        );
        assert_eq!(answer.finish_reason, FinishReason::Stop);
        assert_eq!(answer.text.as_deref(), Some("{\"code\":\"\"}"));
    }

    #[test]
    fn stop_reasons_map_to_finish_reasons() {
        for (stop, finish) in [
            ("max_tokens", FinishReason::Length),
            ("refusal", FinishReason::ContentFilter),
            ("pause_turn", FinishReason::Other),
        ] {
            let response: Response = serde_json::from_value(json!({
                "content": [], "stop_reason": stop, "usage": { "input_tokens": 1, "output_tokens": 1 },
            }))
            .unwrap();
            assert_eq!(Answer::from(response).finish_reason, finish);
        }
    }
}
