use std::collections::HashMap;

use crate::domain::{Secret, Usd};

use super::pricing::Pricing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
    Google,
}

impl Provider {
    fn parse(name: &str) -> Option<Provider> {
        match name {
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::OpenAi),
            "google" => Some(Provider::Google),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
            Provider::Google => "google",
        }
    }

    /// The environment variable holding this provider's key.
    pub fn key_variable(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Google => "GOOGLE_GENERATIVE_AI_API_KEY",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelConfig {
    pub provider: Provider,
    pub model: String,
    pub api_key: Secret,
    pub base_url: Option<String>,
    pub max_output_tokens: u32,
    pub pricing: Pricing,
}

impl ModelConfig {
    /// `provider/model`, as recorded in the trajectory.
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider.name(), self.model)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceConfig {
    pub model: ModelConfig,
    /// Spend cap per UTC day.
    pub budget: Usd,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_000;

/// Inference settings from environment-style variables.
pub fn read(env: &HashMap<String, String>) -> Result<InferenceConfig, ConfigError> {
    let fail = |message: &str| ConfigError(message.to_owned());
    let selection = env.get("MODEL").map_or(DEFAULT_MODEL, String::as_str);
    let (provider, model) = selection
        .split_once('/')
        .and_then(|(provider, model)| Some((Provider::parse(provider)?, model.trim())))
        .filter(|(_, model)| !model.is_empty())
        .ok_or_else(|| fail("MODEL must be anthropic/<model>, openai/<model>, or google/<model>"))?;

    let key = provider.key_variable();
    let api_key = env
        .get(key)
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| ConfigError(format!("Set {key} for MODEL={selection}")))?;

    let max_output_tokens = match env.get("MODEL_MAX_OUTPUT_TOKENS") {
        None => DEFAULT_MAX_OUTPUT_TOKENS,
        Some(value) => value
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| fail("MODEL_MAX_OUTPUT_TOKENS must be a positive integer"))?,
    };

    let budget = match env.get("MODEL_BUDGET_USD") {
        None => Usd::new(1.0),
        Some(value) => value
            .parse()
            .ok()
            .and_then(Usd::new)
            .filter(|budget| budget.get() > 0.0),
    }
    .ok_or_else(|| fail("MODEL_BUDGET_USD must be a positive number"))?;

    let id = format!("{}/{model}", provider.name());
    let pricing = match env.get("MODEL_PRICING") {
        Some(rates) => Pricing::parse(rates).ok_or_else(|| {
            fail("MODEL_PRICING must be 2 or 4 nonnegative numbers: input,output[,cacheRead,cacheWrite]")
        })?,
        None => Pricing::known(&id).ok_or_else(|| {
            ConfigError(format!(
                "Set MODEL_PRICING=\"input,output,cacheRead,cacheWrite\" (USD per million tokens) for MODEL={id}"
            ))
        })?,
    };

    Ok(InferenceConfig {
        model: ModelConfig {
            provider,
            model: model.to_owned(),
            api_key: Secret::new(api_key),
            base_url: env.get("MODEL_BASE_URL").cloned(),
            max_output_tokens,
            pricing,
        },
        budget,
    })
}
