//! Runs model-generated source in a sandbox and records what it does.

pub mod capabilities;
mod sandbox;

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::domain::{Evaluator, EvaluatorStopped, Seq, Trajectory};

pub use capabilities::Capabilities;
pub use sandbox::{Limits, Sandbox, SandboxError, SandboxHost, SourceChecker};

/// The evaluator over a sandbox. A task owns the sandbox and appends its
/// output and results to the trajectory as they happen.
pub struct SandboxEvaluator {
    starts: mpsc::Sender<Start>,
}

struct Start {
    eval: Seq,
    code: String,
    started: oneshot::Sender<()>,
}

impl SandboxEvaluator {
    pub fn spawn(sandbox: Sandbox, log: Arc<Trajectory>, debug: fn(&str)) -> Self {
        let (starts, requests) = mpsc::channel(16);
        tokio::spawn(run(sandbox, log, requests, debug));
        SandboxEvaluator { starts }
    }
}

impl Evaluator for SandboxEvaluator {
    async fn start(&self, eval: Seq, code: String) -> Result<(), EvaluatorStopped> {
        let (started, done) = oneshot::channel();
        self.starts
            .send(Start { eval, code, started })
            .await
            .map_err(|_| EvaluatorStopped)?;
        done.await.map_err(|_| EvaluatorStopped)
    }
}

/// Starts evaluations and delivers timers and host calls back to the
/// sandbox, until the evaluator is dropped.
async fn run(mut sandbox: Sandbox, log: Arc<Trajectory>, mut starts: mpsc::Receiver<Start>, debug: fn(&str)) {
    loop {
        tokio::select! {
            start = starts.recv() => {
                let Some(Start { eval, code, started }) = start else { return };
                sandbox.evaluate(eval, &code).await;
                record(&log, &mut sandbox, debug);
                let _ = started.send(());
            }
            Some(wakeup) = sandbox.next_wakeup(), if sandbox.is_waiting() => {
                sandbox.wake(wakeup).await;
                record(&log, &mut sandbox, debug);
            }
        }
    }
}

fn record(log: &Trajectory, sandbox: &mut Sandbox, debug: fn(&str)) {
    for event in sandbox.take_events() {
        if let Err(error) = log.append(event) {
            debug(&format!("observation not recorded: {error}"));
        }
    }
}
