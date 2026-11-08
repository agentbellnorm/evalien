//! The agent's composition root: which implementation fills each port.

use std::sync::Arc;

use anyhow::Context;
use evalien::{
    chat::{self, Matrix},
    config::{self, Human},
    connectors::Connectors,
    db::{self, Access, SqliteStore},
    domain::{Gateway, TRAJECTORY_TABLE, Timestamp, Trajectory},
    evaluation::{
        Capabilities, Limits, Sandbox, SandboxEvaluator, SandboxHost,
        capabilities::{Fetch, Sql},
    },
    harness::{Harness, Stopped, policy::IDLE_DELAYS},
    inference::{self, Budgeted, spent_on_day},
    lifecycle,
    metrics::{self, Metrics},
    system_prompt,
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

    // Today's spend so far comes from the recorded calls, so restarts don't reset it.
    let now = Timestamp::now();
    let spent_today = spent_on_day(&log.read_all()?, now);
    let budget = config.inference.budget;
    let model = Budgeted::daily(inference::connect(config.inference.model)?, budget, spent_today);

    // The agent's own handle on the database, with the trajectory read-only.
    // The credential gateway, when there are connections to sign requests for.
    let connection_names: Vec<String> = config.connections.iter().map(|c| c.name.clone()).collect();
    let gateway: Option<Arc<dyn Gateway>> =
        (!config.connections.is_empty()).then(|| Arc::new(Connectors::new(config.connections)) as Arc<dyn Gateway>);

    let mut capabilities = Capabilities::none()
        .with_sql(Sql::open(&config.db_path, TRAJECTORY_TABLE)?)
        .with_fetch(Fetch::new(gateway.clone()).map_err(anyhow::Error::msg)?);
    if let Some(gateway) = &gateway {
        capabilities = capabilities.with_connections(gateway.clone());
    }
    let sandbox = Sandbox::new(SandboxHost::new()?, capabilities, limits.clone()).await?;
    let evaluator = SandboxEvaluator::spawn(sandbox, log.clone(), debug);

    // The trajectory as it grows: on the terminal, or in the pod's log.
    tokio::spawn(display::show(log.clone(), log.last_seq()));
    if let Some(addr) = config.metrics {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("serving metrics on {addr}"))?;
        let mut metrics = Metrics::new(budget, spent_today, now);
        if let Some(gateway) = &gateway {
            metrics = metrics.with_gateway(gateway.clone());
        }
        let metrics = Arc::new(metrics);
        tokio::spawn(metrics::follow(log.clone(), metrics.clone(), log.last_seq(), debug));
        tokio::spawn(metrics::serve(listener, metrics, debug));
    }
    let channel = match config.human {
        Human::Terminal => {
            let typed = log.clone();
            tokio::spawn(async move {
                if let Err(error) = input::forward_lines(tokio::io::BufReader::new(tokio::io::stdin()), &typed).await {
                    debug(&format!("input stopped: {error}"));
                }
            });
            "the terminal running this process"
        }
        Human::Matrix(matrix) => {
            let chat = Matrix::connect(matrix).await.context("connecting to Matrix")?;
            tokio::spawn(chat::relay(chat, log.clone(), debug));
            "a Matrix chat room"
        }
    };

    let instructions = system_prompt::build(&system_prompt::Facts {
        channel,
        table: TRAJECTORY_TABLE,
        idle_delays: &IDLE_DELAYS,
        soft_limit: limits.soft_slice,
        memory_mib: limits.memory_bytes >> 20,
        connections: &connection_names,
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

/// The harness only stops on failure: a spent budget pauses it instead.
fn report(stopped: Stopped) -> u8 {
    eprintln!("\nevalien stopped: {stopped}");
    1
}
