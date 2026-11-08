//! The shared vocabulary. Every other module depends on this one and never on
//! each other; the composition roots wire implementations together.

pub mod codec;
mod event;
mod gateway;
mod ports;
mod trajectory;

pub use event::{Entry, Event, FinishReason, Generation, Outcome, Secret, Seq, Timestamp, Usage, Usd};
pub use gateway::{
    Attachment, Connecting, ConnectionInfo, ConnectionKind, ConnectionStatus, Gateway, Origin, RequestCounts,
    RequestOutcome,
};
pub use ports::{Evaluator, EvaluatorStopped, GenerateError, Model, Prompt, Reply};
pub use trajectory::{RecordStore, StoreError, StoredRecord, Trajectory, TrajectoryError};

/// The table holding the trajectory, for the runtime, the report and the agent's own queries.
pub const TRAJECTORY_TABLE: &str = "trajectory";
