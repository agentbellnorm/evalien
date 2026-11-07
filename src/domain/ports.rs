//! What the harness needs from the outside world. Implementations live in
//! their own modules and are chosen by the composition roots.

use std::future::Future;

use super::event::{Generation, Seq, Usd};

/// What the model sees: fixed instructions, then the trajectory as text blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub instructions: String,
    pub blocks: Vec<String>,
}

/// A completed model call: JavaScript source, or empty to do nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub generation: Generation,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum GenerateError {
    /// The call was billed but produced no usable source.
    #[error("{reason}")]
    Unusable { generation: Generation, reason: String },
    /// The call failed before producing a billed reply.
    #[error("generation failed: {0}")]
    Failed(String),
    /// This process has spent its budget.
    #[error("spent {spent} of this process's {budget} budget")]
    BudgetSpent { spent: Usd, budget: Usd },
}

pub trait Model: Send + Sync {
    fn generate(&self, prompt: &Prompt) -> impl Future<Output = Result<Reply, GenerateError>> + Send;
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("the evaluator has stopped")]
pub struct EvaluatorStopped;

/// Runs model-generated source.
pub trait Evaluator: Send + Sync {
    /// Start evaluating `code`, the source of eval entry `eval`. Output and the
    /// result are appended to the trajectory as they happen. Returns once the
    /// code has run up to its first pause, so a result that needed no waiting
    /// is already recorded.
    fn start(&self, eval: Seq, code: String) -> impl Future<Output = Result<(), EvaluatorStopped>> + Send;
}
