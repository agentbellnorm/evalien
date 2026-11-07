use std::{
    fmt,
    iter::Sum,
    ops::Add,
    str::FromStr,
    time::{Duration, SystemTime},
};

/// A position in the trajectory. The first entry is 1; zero means "before everything".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
    pub const ZERO: Seq = Seq(0);

    pub const fn new(n: u64) -> Self {
        Seq(n)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// When the store recorded an entry. Displays as RFC 3339 with milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(SystemTime);

impl Timestamp {
    pub fn now() -> Self {
        Timestamp(SystemTime::now())
    }

    pub fn as_system_time(self) -> SystemTime {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        humantime::format_rfc3339_millis(self.0).fmt(f)
    }
}

impl FromStr for Timestamp {
    type Err = humantime::TimestampError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        humantime::parse_rfc3339(s).map(Timestamp)
    }
}

/// An amount in US dollars. Always finite and nonnegative.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct Usd(f64);

impl Usd {
    pub const ZERO: Usd = Usd(0.0);

    pub fn new(amount: f64) -> Option<Self> {
        (amount.is_finite() && amount >= 0.0).then_some(Usd(amount))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl Add for Usd {
    type Output = Usd;

    fn add(self, other: Usd) -> Usd {
        Usd(self.0 + other.0)
    }
}

impl Sum for Usd {
    fn sum<I: Iterator<Item = Usd>>(iter: I) -> Usd {
        iter.fold(Usd::ZERO, Add::add)
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${:.4}", self.0)
    }
}

/// How an evaluation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Return,
    Throw,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Return => "return",
            Outcome::Throw => "throw",
        }
    }
}

/// Why a model call ended. Only `Stop` produced usable source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Error,
    Other,
    Unknown,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ContentFilter => "content-filter",
            FinishReason::ToolCalls => "tool-calls",
            FinishReason::Error => "error",
            FinishReason::Other => "other",
            FinishReason::Unknown => "unknown",
        }
    }
}

/// Tokens by billing class, and their cost.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost: Usd,
}

impl Usage {
    pub fn input_tokens(&self) -> u64 {
        self.uncached_input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// What one model call cost and how it ended.
#[derive(Clone, Debug, PartialEq)]
pub struct Generation {
    /// `provider/model`.
    pub model: String,
    pub latency: Duration,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

/// An observation at the runtime boundary. Values are captured as text.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A fresh runtime.
    Start,
    /// Source accepted for evaluation. Its sequence number identifies the evaluation.
    Eval {
        code: String,
    },
    /// Input from a human.
    Stdin {
        text: String,
    },
    /// Output from running code.
    Stdout {
        text: String,
    },
    Stderr {
        text: String,
    },
    /// An evaluation settled. `eval` is the sequence number of its `Eval` entry.
    Result {
        eval: Seq,
        outcome: Outcome,
        text: String,
    },
    /// A model call. `through` is the last entry its prompt included.
    Generation {
        through: Seq,
        generation: Generation,
    },
}

/// An event as recorded in the trajectory.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: Seq,
    pub timestamp: Timestamp,
    pub event: Event,
}
