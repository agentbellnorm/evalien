//! Prometheus metrics, read off the trajectory. Every model call is already a
//! recorded `generation` event with its provider, model, tokens, cost and
//! latency, so this is one more follower of the log, like the display and the
//! chat relay. Nothing else in the agent knows it exists.
//!
//! Counters start at zero with the process, as Prometheus expects. Today's
//! spend starts from the recorded calls, so it matches the budget.

mod exposition;
mod http;

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

use crate::domain::{ConnectionStatus, Entry, Event, Gateway, Generation, Outcome, Seq, Timestamp, Trajectory, Usd};

pub use exposition::Histogram;
pub use http::serve;

/// Model-call latencies, in seconds.
const CALL_BUCKETS: [f64; 10] = [0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0];
/// How long evaluations take to settle, in seconds.
const EVAL_BUCKETS: [f64; 10] = [0.01, 0.1, 0.5, 1.0, 5.0, 30.0, 60.0, 300.0, 1800.0, 3600.0];

/// A model, as `provider` and `model` labels.
type ModelKey = (String, String);

pub struct Metrics {
    budget: Usd,
    state: Mutex<State>,
    /// Connections, whose request counts the gateway keeps.
    gateway: Option<Arc<dyn Gateway>>,
}

struct State {
    calls: BTreeMap<(ModelKey, &'static str), u64>,
    tokens: BTreeMap<(ModelKey, &'static str), u64>,
    cost: BTreeMap<ModelKey, f64>,
    call_seconds: BTreeMap<ModelKey, Histogram>,
    events: BTreeMap<&'static str, u64>,
    eval_seconds: BTreeMap<&'static str, Histogram>,
    /// When each evaluation still pending started.
    started: HashMap<Seq, SystemTime>,
    /// Spend on one UTC day.
    today: (u64, f64),
}

impl Metrics {
    /// `spent_today` is what the recorded calls cost so far today.
    pub fn new(budget: Usd, spent_today: Usd, now: Timestamp) -> Self {
        let state = State {
            calls: BTreeMap::new(),
            tokens: BTreeMap::new(),
            cost: BTreeMap::new(),
            call_seconds: BTreeMap::new(),
            events: BTreeMap::new(),
            eval_seconds: BTreeMap::new(),
            started: HashMap::new(),
            today: (now.utc_day(), spent_today.get()),
        };
        Metrics {
            budget,
            state: Mutex::new(state),
            gateway: None,
        }
    }

    /// Also report on the gateway's connections.
    pub fn with_gateway(self, gateway: Arc<dyn Gateway>) -> Self {
        Metrics {
            gateway: Some(gateway),
            ..self
        }
    }

    pub fn observe(&self, entry: &Entry) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state.events.entry(entry.event.kind()).or_default() += 1;
        let at = entry.timestamp.as_system_time();
        match &entry.event {
            Event::Generation { generation, .. } => state.call(generation, entry.timestamp),
            Event::Eval { .. } => {
                state.started.insert(entry.seq, at);
            }
            Event::Result { eval, outcome, .. } => {
                if let Some(started) = state.started.remove(eval) {
                    let seconds = at.duration_since(started).unwrap_or_default().as_secs_f64();
                    let outcome = match outcome {
                        Outcome::Return => "return",
                        Outcome::Throw => "throw",
                    };
                    state
                        .eval_seconds
                        .entry(outcome)
                        .or_insert_with(|| Histogram::new(&EVAL_BUCKETS))
                        .observe(seconds);
                }
            }
            _ => {}
        }
    }

    /// The metrics in Prometheus's text format.
    pub fn render(&self, now: Timestamp) -> String {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let model = |(provider, model): &ModelKey| vec![("provider", provider.clone()), ("model", model.clone())];
        let with = |key: &ModelKey, name: &'static str, value: &str| {
            let mut labels = model(key);
            labels.push((name, value.to_owned()));
            labels
        };

        let mut out = exposition::Writer::default();
        out.counter(
            "evalien_model_calls_total",
            "Model calls, by how they ended.",
            state
                .calls
                .iter()
                .map(|((key, finish), n)| (with(key, "finish_reason", finish), *n as f64)),
        );
        out.counter(
            "evalien_model_tokens_total",
            "Tokens by billing class: input_uncached, input_cache_read, input_cache_write, output, output_reasoning.",
            state
                .tokens
                .iter()
                .map(|((key, class), n)| (with(key, "class", class), *n as f64)),
        );
        out.counter(
            "evalien_model_cost_usd_total",
            "What model calls cost, in US dollars.",
            state.cost.iter().map(|(key, usd)| (model(key), *usd)),
        );
        out.histogram(
            "evalien_model_call_duration_seconds",
            "How long model calls took.",
            state
                .call_seconds
                .iter()
                .map(|(key, histogram)| (model(key), histogram)),
        );
        out.gauge(
            "evalien_budget_daily_usd",
            "The daily model budget, in US dollars.",
            [(vec![], self.budget.get())],
        );
        let spent = if state.today.0 == now.utc_day() {
            state.today.1
        } else {
            0.0
        };
        out.gauge(
            "evalien_budget_spent_today_usd",
            "What model calls cost today (UTC), in US dollars.",
            [(vec![], spent)],
        );
        out.counter(
            "evalien_events_total",
            "Trajectory events, by type.",
            state
                .events
                .iter()
                .map(|(kind, n)| (vec![("type", kind.to_string())], *n as f64)),
        );
        out.histogram(
            "evalien_evaluation_duration_seconds",
            "How long evaluations took to settle, by outcome.",
            state
                .eval_seconds
                .iter()
                .map(|(outcome, histogram)| (vec![("outcome", outcome.to_string())], histogram)),
        );
        out.gauge(
            "evalien_evaluations_pending",
            "Evaluations started and not yet settled.",
            [(vec![], state.started.len() as f64)],
        );
        if let Some(gateway) = &self.gateway {
            let connections = gateway.connections();
            out.counter(
                "evalien_connection_requests_total",
                "Requests through each connection, by outcome: succeeded, rejected (401/403), client_error, server_error, failed.",
                connections.iter().flat_map(|connection| {
                    let r = connection.requests;
                    [
                        ("succeeded", r.succeeded),
                        ("rejected", r.rejected),
                        ("client_error", r.client_errors),
                        ("server_error", r.server_errors),
                        ("failed", r.failed),
                    ]
                    .map(|(outcome, n)| {
                        let labels = vec![("connection", connection.name.clone()), ("outcome", outcome.to_owned())];
                        (labels, n as f64)
                    })
                }),
            );
            out.gauge(
                "evalien_connection_connected",
                "Whether each connection is connected (1) or needs a login (0).",
                connections.iter().map(|connection| {
                    let labels = vec![
                        ("connection", connection.name.clone()),
                        ("kind", connection.kind.as_str().to_owned()),
                    ];
                    (
                        labels,
                        f64::from(u8::from(connection.status == ConnectionStatus::Connected)),
                    )
                }),
            );
        }
        out.finish()
    }
}

impl State {
    fn call(&mut self, generation: &Generation, at: Timestamp) {
        let key = split(&generation.model);
        let usage = &generation.usage;
        *self
            .calls
            .entry((key.clone(), generation.finish_reason.as_str()))
            .or_default() += 1;
        for (class, n) in [
            ("input_uncached", usage.uncached_input_tokens),
            ("input_cache_read", usage.cache_read_tokens),
            ("input_cache_write", usage.cache_write_tokens),
            ("output", usage.output_tokens),
            ("output_reasoning", usage.reasoning_tokens),
        ] {
            *self.tokens.entry((key.clone(), class)).or_default() += n;
        }
        *self.cost.entry(key.clone()).or_default() += usage.cost.get();
        self.call_seconds
            .entry(key)
            .or_insert_with(|| Histogram::new(&CALL_BUCKETS))
            .observe(generation.latency.as_secs_f64());

        let day = at.utc_day();
        if self.today.0 != day {
            self.today = (day, 0.0);
        }
        self.today.1 += usage.cost.get();
    }
}

/// `provider/model` as recorded, split into labels.
fn split(model: &str) -> ModelKey {
    match model.split_once('/') {
        Some((provider, name)) => (provider.to_owned(), name.to_owned()),
        None => ("unknown".to_owned(), model.to_owned()),
    }
}

/// Count everything appended after `cursor`. Runs until the process ends.
pub async fn follow(log: Arc<Trajectory>, metrics: Arc<Metrics>, mut cursor: Seq, debug: fn(&str)) {
    loop {
        let latest = log.wait_after(cursor).await;
        match log.read(cursor, latest) {
            Ok(entries) => entries.iter().for_each(|entry| metrics.observe(entry)),
            Err(error) => debug(&format!("metrics: {error}")),
        }
        cursor = latest;
    }
}
