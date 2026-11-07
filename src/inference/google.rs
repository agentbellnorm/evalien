//! The Gemini generateContent API with a JSON response schema.

use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{FinishReason, GenerateError, Model, Prompt, Reply};

use super::{Answer, ModelConfig, endpoint, pricing::Tokens, send, settle, source_schema};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct Google {
    config: ModelConfig,
    http: reqwest::Client,
}

impl Google {
    pub fn new(config: ModelConfig, http: reqwest::Client) -> Self {
        Google { config, http }
    }
}

impl Model for Google {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let base = self.config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        let url = endpoint(base, &format!("models/{}:generateContent", self.config.model));
        let request = self
            .http
            .post(url)
            .header("x-goog-api-key", self.config.api_key.expose())
            .json(&request_body(&self.config, prompt));
        let started = Instant::now();
        let response: Response = send(request).await?;
        settle(&self.config, started.elapsed(), response.into())
    }
}

fn request_body(config: &ModelConfig, prompt: &Prompt) -> Value {
    let parts: Vec<Value> = prompt.blocks.iter().map(|text| json!({ "text": text })).collect();
    json!({
        "systemInstruction": { "parts": [{ "text": prompt.instructions }] },
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": {
            "maxOutputTokens": config.max_output_tokens,
            "responseMimeType": "application/json",
            "responseJsonSchema": source_schema(),
        },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    #[serde(default)]
    candidates: Vec<Candidate>,
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: Option<Content>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct UsageMetadata {
    prompt_token_count: u64,
    cached_content_token_count: u64,
    candidates_token_count: u64,
    thoughts_token_count: u64,
}

impl From<Response> for Answer {
    fn from(response: Response) -> Answer {
        let candidate = response.candidates.into_iter().next();
        let finish_reason = match candidate.as_ref().and_then(|c| c.finish_reason.as_deref()) {
            Some("STOP") => FinishReason::Stop,
            Some("MAX_TOKENS") => FinishReason::Length,
            Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY") => {
                FinishReason::ContentFilter
            }
            Some("MALFORMED_FUNCTION_CALL") => FinishReason::Error,
            Some(_) => FinishReason::Other,
            None => FinishReason::Unknown,
        };
        let text = candidate
            .and_then(|c| c.content)
            .map(|content| {
                content
                    .parts
                    .into_iter()
                    .filter(|part| !part.thought)
                    .filter_map(|part| part.text)
                    .collect::<String>()
            })
            .filter(|text| !text.is_empty());
        let usage = response.usage_metadata.unwrap_or_default();
        Answer {
            tokens: Tokens {
                uncached_input: usage
                    .prompt_token_count
                    .saturating_sub(usage.cached_content_token_count),
                cache_read: usage.cached_content_token_count,
                cache_write: 0,
                // Thinking is billed as output.
                output: usage.candidates_token_count + usage.thoughts_token_count,
                reasoning: usage.thoughts_token_count,
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
    fn requests_json_source_with_instructions_and_ordered_parts() {
        let prompt = Prompt {
            instructions: "be good".into(),
            blocks: vec!["a".into(), "b".into()],
        };
        let body = request_body(&config(Provider::Google, "gemini-2.5-flash"), &prompt);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be good");
        assert_eq!(body["contents"][0]["parts"][1]["text"], "b");
        assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
    }

    #[test]
    fn usage_counts_thoughts_as_output_and_skips_thought_parts() {
        let response: Response = serde_json::from_value(json!({
            "candidates": [{
                "content": { "parts": [{ "text": "hmm", "thought": true }, { "text": "{\"code\":\"\"}" }] },
                "finishReason": "STOP",
            }],
            "usageMetadata": { "promptTokenCount": 100, "cachedContentTokenCount": 40, "candidatesTokenCount": 10, "thoughtsTokenCount": 5 },
        }))
        .unwrap();
        let answer = Answer::from(response);
        assert_eq!(
            answer.tokens,
            Tokens {
                uncached_input: 60,
                cache_read: 40,
                cache_write: 0,
                output: 15,
                reasoning: 5
            }
        );
        assert_eq!(answer.finish_reason, FinishReason::Stop);
        assert_eq!(answer.text.as_deref(), Some("{\"code\":\"\"}"));
    }
}
