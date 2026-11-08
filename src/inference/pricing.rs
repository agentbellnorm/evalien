use crate::domain::{Usage, Usd};

/// USD per million tokens, by billing class.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Tokens a call used, before pricing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Pricing {
    /// Only rates confirmed for the model ID. Other models need MODEL_PRICING.
    pub fn known(model_id: &str) -> Option<Pricing> {
        match model_id {
            "anthropic/claude-sonnet-5-5" => Some(Pricing {
                input: 2.0,
                output: 10.0,
                cache_read: 0.2,
                cache_write: 2.5,
            }),
            "anthropic/claude-opus-5-5" => Some(Pricing {
                input: 4.0,
                output: 20.0,
                cache_read: 0.2,
                cache_write: 5.0,
            }),
            _ => None,
        }
    }

    /// Parse "input,output[,cacheRead,cacheWrite]". Cache rates default to the input rate.
    pub fn parse(rates: &str) -> Option<Pricing> {
        let rates: Vec<f64> = rates
            .split(',')
            .map(|part| part.trim().parse().ok())
            .collect::<Option<_>>()?;
        if rates.iter().any(|rate| !rate.is_finite() || *rate < 0.0) {
            return None;
        }
        match rates[..] {
            [input, output] => Some(Pricing {
                input,
                output,
                cache_read: input,
                cache_write: input,
            }),
            [input, output, cache_read, cache_write] => Some(Pricing {
                input,
                output,
                cache_read,
                cache_write,
            }),
            _ => None,
        }
    }

    pub fn usage(&self, tokens: Tokens) -> Usage {
        let millionths = tokens.uncached_input as f64 * self.input
            + tokens.cache_read as f64 * self.cache_read
            + tokens.cache_write as f64 * self.cache_write
            + tokens.output as f64 * self.output;
        Usage {
            uncached_input_tokens: tokens.uncached_input,
            cache_read_tokens: tokens.cache_read,
            cache_write_tokens: tokens.cache_write,
            output_tokens: tokens.output,
            reasoning_tokens: tokens.reasoning,
            // Rates are checked finite and nonnegative, so the cost is too.
            cost: Usd::new(millionths / 1e6).unwrap_or(Usd::ZERO),
        }
    }
}
