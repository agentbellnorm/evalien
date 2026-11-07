//! The harness: producers append observations to the trajectory, and one
//! consumer decides when to call the model and starts evaluating what it
//! returns. That loop is the whole agent.

pub mod context;
pub mod policy;

use std::{
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::domain::{
    Entry, Evaluator, EvaluatorStopped, Event, GenerateError, Model, Prompt, Seq, Trajectory, TrajectoryError,
};

pub struct Harness<M, E> {
    pub log: Arc<Trajectory>,
    pub model: M,
    pub evaluator: E,
    pub instructions: String,
    /// Minimum spacing between model calls with no human input since the last one.
    pub idle_delays: Vec<Duration>,
    /// Host diagnostics; never part of the trajectory.
    pub debug: fn(&str),
}

/// Why the harness stopped.
#[derive(Debug, thiserror::Error)]
pub enum Stopped {
    #[error(transparent)]
    Trajectory(#[from] TrajectoryError),
    #[error(transparent)]
    Generation(#[from] GenerateError),
    #[error(transparent)]
    Evaluator(#[from] EvaluatorStopped),
}

/// What the consumer knows between iterations.
struct Attention {
    /// Everything up to here has been observed.
    cursor: Seq,
    /// Evaluations started and not yet settled.
    pending: usize,
    /// Consecutive model calls without human input.
    quiet: usize,
    last_call: Option<Instant>,
}

/// What a batch of observations asks for.
#[derive(Default)]
struct Observed {
    wakes: bool,
    from_human: bool,
}

impl Attention {
    fn observe(&mut self, entries: &[Entry]) -> Observed {
        let mut observed = Observed::default();
        for entry in entries {
            match entry.event {
                Event::Eval { .. } => self.pending += 1,
                Event::Result { .. } => self.pending = self.pending.saturating_sub(1),
                _ => {}
            }
            observed.wakes |= policy::wakes(&entry.event, self.pending);
            observed.from_human |= policy::from_human(&entry.event);
        }
        observed
    }
}

impl<M: Model, E: Evaluator> Harness<M, E> {
    /// Run until something stops the harness, and return why.
    pub async fn run(self) -> Stopped {
        match self.consume().await {
            Ok(never) => match never {},
            Err(stopped) => stopped,
        }
    }

    async fn consume(&self) -> Result<Infallible, Stopped> {
        let mut attention = Attention {
            cursor: self.log.last_seq(),
            pending: 0,
            quiet: 0,
            last_call: None,
        };
        self.log.append(Event::Start)?;
        loop {
            let observed = self.next_observations(&mut attention).await?;
            if !observed.wakes {
                continue;
            }
            attention.quiet = if observed.from_human { 0 } else { attention.quiet + 1 };
            if attention.quiet > 0 {
                self.pace(&mut attention).await?;
            }

            // A fixed prefix: anything arriving during generation stays after
            // the cursor for the next iteration.
            let through = attention.cursor;
            (self.debug)(&format!("thinking... (through event {through})"));
            attention.last_call = Some(Instant::now());
            let prompt = Prompt {
                instructions: self.instructions.clone(),
                blocks: context::prompt_window(&self.log, through)?,
            };
            let reply = match self.model.generate(&prompt).await {
                Ok(reply) => reply,
                Err(error) => {
                    if let GenerateError::Unusable { generation, .. } = &error {
                        self.log.append(Event::Generation {
                            through,
                            generation: generation.clone(),
                        })?;
                    }
                    return Err(error.into());
                }
            };
            self.log.append(Event::Generation {
                through,
                generation: reply.generation,
            })?;
            // No code ends the turn. The next waking observation starts another.
            if reply.code.trim().is_empty() {
                (self.debug)("idle until the next event");
                continue;
            }
            let eval = self.log.append(Event::Eval {
                code: reply.code.clone(),
            })?;
            self.evaluator.start(eval.seq, reply.code).await?;
        }
    }

    async fn next_observations(&self, attention: &mut Attention) -> Result<Observed, Stopped> {
        let latest = self.log.wait_after(attention.cursor).await;
        let entries = self.log.read(attention.cursor, latest)?;
        attention.cursor = latest;
        Ok(attention.observe(&entries))
    }

    /// Hold a quiet call until its spacing has passed, or human input arrives.
    async fn pace(&self, attention: &mut Attention) -> Result<(), Stopped> {
        let Some(last_call) = attention.last_call else {
            return Ok(());
        };
        let deadline = last_call + policy::idle_delay(attention.quiet, &self.idle_delays);
        let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        (self.debug)(&format!(
            "pacing: quiet call {}, waiting {}s unless input arrives",
            attention.quiet,
            wait.as_secs_f64().ceil()
        ));
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline.into()) => return Ok(()),
                observed = self.next_observations(attention) => {
                    if observed?.from_human {
                        attention.quiet = 0;
                        return Ok(());
                    }
                }
            }
        }
    }
}
