use std::sync::{Mutex, PoisonError};

use crate::domain::{GenerateError, Model, Prompt, Reply, Usd};

/// Refuses to start a call once this process has spent its budget. The call
/// that crosses the limit completes. Spend lives in memory, out of reach of
/// evaluated code that edits stored history.
pub struct Budgeted<M> {
    inner: M,
    budget: Usd,
    spent: Mutex<Usd>,
}

impl<M> Budgeted<M> {
    pub fn new(inner: M, budget: Usd) -> Self {
        Budgeted {
            inner,
            budget,
            spent: Mutex::new(Usd::ZERO),
        }
    }

    pub fn spent(&self) -> Usd {
        *self.spent.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn charge(&self, cost: Usd) {
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        *spent = *spent + cost;
    }
}

impl<M: Model> Model for Budgeted<M> {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let spent = self.spent();
        if spent >= self.budget {
            return Err(GenerateError::BudgetSpent {
                spent,
                budget: self.budget,
            });
        }
        let result = self.inner.generate(prompt).await;
        match &result {
            Ok(reply) => self.charge(reply.generation.usage.cost),
            Err(GenerateError::Unusable { generation, .. }) => self.charge(generation.usage.cost),
            Err(_) => {}
        }
        result
    }
}
