//! The agent's composition root: which implementation fills each port.

use std::sync::Arc;

use anyhow::Context;
use evalien::{
    config,
    db::{self, Access, SqliteStore},
    domain::{GenerateError, TRAJECTORY_TABLE, Trajectory},
    evaluation::{
        Capabilities, Limits, Sandbox, SandboxEvaluator, SandboxHost,
        capabilities::{Fetch, Sql},
    },
    harness::{Harness, Stopped, policy::IDLE_DELAYS},
    inference::{self, Budgeted},
    lifecycle, system_prompt,
    terminal::{display, format::debug, input},
};

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("evalien: {error:#}");
            1
        }
    };
    // Exit explicitly: the stdin reader blocks a thread that would otherwise
    // keep the runtime from shutting down. Every append is already durable.
    std::process::exit(code.into())
}

async fn run() -> anyhow::Result<u8> {
    let config = config::read(&config::environment())?;
    let limits = Limits::default();

    let connection = db::open(&config.db_path, Access::ReadWrite)
        .with_context(|| format!("opening {}", config.db_path.display()))?;
    let log = Arc::new(Trajectory::new(SqliteStore::new(connection, TRAJECTORY_TABLE)?)?);

    let model = Budgeted::new(inference::connect(config.inference.model)?, config.inference.budget);

    // The agent's own handle on the database, with the trajectory read-only.
    let capabilities = Capabilities::none()
        .with_sql(Sql::open(&config.db_path, TRAJECTORY_TABLE)?)
        .with_fetch(Fetch::new().map_err(anyhow::Error::msg)?);
    let sandbox = Sandbox::new(SandboxHost::new()?, capabilities, limits.clone()).await?;
    let evaluator = SandboxEvaluator::spawn(sandbox, log.clone(), debug);

    tokio::spawn(display::show(log.clone(), log.last_seq()));
    let typed = log.clone();
    tokio::spawn(async move {
        if let Err(error) = input::forward_lines(tokio::io::BufReader::new(tokio::io::stdin()), &typed).await {
            debug(&format!("input stopped: {error}"));
        }
    });

    let instructions = system_prompt::build(&system_prompt::Facts {
        table: TRAJECTORY_TABLE,
        idle_delays: &IDLE_DELAYS,
        soft_limit: limits.soft_slice,
        memory_mib: limits.memory_bytes >> 20,
    });
    let harness = Harness {
        log,
        model,
        evaluator,
        instructions,
        idle_delays: IDLE_DELAYS.to_vec(),
        debug,
    };

    tokio::select! {
        stopped = harness.run() => Ok(report(stopped)),
        signal = lifecycle::stop_signal() => {
            let signal = signal?;
            eprintln!("\n{signal}: stopping.");
            Ok(signal.exit_code())
        }
    }
}

/// Spending the budget is an expected stop; anything else is a failure.
fn report(stopped: Stopped) -> u8 {
    match stopped {
        Stopped::Generation(GenerateError::BudgetSpent { .. }) => {
            eprintln!("\n{stopped}. Stopping.");
            2
        }
        _ => {
            eprintln!("\nevalien stopped: {stopped}");
            1
        }
    }
}
