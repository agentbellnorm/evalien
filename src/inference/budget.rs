use std::sync::{Mutex, PoisonError};

use crate::domain::{Entry, Event, GenerateError, Model, Prompt, Reply, Timestamp, Usd};

/// Refuses to start a call once today's budget (UTC) is spent. The call that
/// crosses the limit completes. Earlier runs' spend comes from the recorded
/// generations, which evaluated code can read but not change, so restarting
/// the process doesn't reset the budget.
pub struct Budgeted<M> {
    inner: M,
    daily: Usd,
    spent: Mutex<DaySpend>,
    clock: fn() -> Timestamp,
}

/// Spend so far on one UTC day.
#[derive(Clone, Copy)]
struct DaySpend {
    day: u64,
    spent: Usd,
}

impl<M> Budgeted<M> {
    /// `spent_today` is what earlier runs spent today; see [`spent_on_day`].
    pub fn daily(inner: M, daily: Usd, spent_today: Usd) -> Self {
        Self::with_clock(inner, daily, spent_today, Timestamp::now)
    }

    pub fn with_clock(inner: M, daily: Usd, spent_today: Usd, clock: fn() -> Timestamp) -> Self {
        let today = DaySpend {
            day: clock().utc_day(),
            spent: spent_today,
        };
        Budgeted {
            inner,
            daily,
            spent: Mutex::new(today),
            clock,
        }
    }

    /// What has been spent today, starting over at midnight UTC.
    pub fn spent_today(&self) -> Usd {
        self.today().spent
    }

    fn today(&self) -> DaySpend {
        let mut spend = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        let day = (self.clock)().utc_day();
        if spend.day != day {
            *spend = DaySpend { day, spent: Usd::ZERO };
        }
        *spend
    }

    fn charge(&self, cost: Usd) {
        self.today();
        let mut spend = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        spend.spent = spend.spent + cost;
    }
}

impl<M: Model> Model for Budgeted<M> {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let spent = self.spent_today();
        if spent >= self.daily {
            let resumes_at = (self.clock)().next_utc_midnight();
            return Err(GenerateError::BudgetSpent {
                spent,
                budget: self.daily,
                resumes_at,
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

/// What the recorded model calls cost on the UTC day of `now`.
pub fn spent_on_day(entries: &[Entry], now: Timestamp) -> Usd {
    let day = now.utc_day();
    entries
        .iter()
        .filter(|entry| entry.timestamp.utc_day() == day)
        .filter_map(|entry| match &entry.event {
            Event::Generation { generation, .. } => Some(generation.usage.cost),
            _ => None,
        })
        .sum()
}
