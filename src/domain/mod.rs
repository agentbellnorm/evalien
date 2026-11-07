//! The shared vocabulary. Every other module depends on this one and never on
//! each other; the composition roots wire implementations together.

pub mod codec;
mod event;
mod ports;
mod trajectory;

pub use event::{Entry, Event, FinishReason, Generation, Outcome, Seq, Timestamp, Usage, Usd};
pub use ports::{Evaluator, EvaluatorStopped, GenerateError, Model, Prompt, Reply};
pub use trajectory::{RecordStore, StoreError, StoredRecord, Trajectory, TrajectoryError};

/// The table holding the trajectory, for the runtime, the report and the agent's own queries.
pub const TRAJECTORY_TABLE: &str = "trajectory";
