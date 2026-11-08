//! Everything the agent reads from the environment.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::chat::MatrixConfig;
use crate::connectors::{self, GatewayConfig};
use crate::inference::{self, ConfigError, InferenceConfig};

pub struct Config {
    pub db_path: PathBuf,
    pub inference: InferenceConfig,
    pub human: Human,
    /// Where to serve Prometheus metrics, if anywhere.
    pub metrics: Option<SocketAddr>,
    /// External services the gateway signs the agent's requests to.
    pub gateway: GatewayConfig,
}

/// How the human reaches the agent.
pub enum Human {
    /// Typing into this process's terminal.
    Terminal,
    Matrix(MatrixConfig),
}

pub fn read(env: &HashMap<String, String>) -> Result<Config, ConfigError> {
    let db_path = db_path(env);
    let state_dir = db_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let human = match MatrixConfig::read(env, state_dir).map_err(ConfigError)? {
        Some(matrix) => Human::Matrix(matrix),
        None => Human::Terminal,
    };
    let gateway = connectors::config::read(env, state_dir).map_err(ConfigError)?;
    let metrics = match env.get("METRICS_ADDR").filter(|addr| !addr.is_empty()) {
        Some(addr) => Some(addr.parse().map_err(|e| ConfigError(format!("METRICS_ADDR: {e}")))?),
        None => None,
    };
    Ok(Config {
        inference: inference::config::read(env)?,
        db_path,
        human,
        metrics,
        gateway,
    })
}

pub fn db_path(env: &HashMap<String, String>) -> PathBuf {
    env.get("AGENT_DB_PATH")
        .filter(|path| !path.is_empty())
        .map_or_else(|| PathBuf::from("/data/agent.db"), PathBuf::from)
}

/// The process environment over the variables in `./.env`, if there is one.
pub fn environment() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = dotenvy::from_filename_iter(".env")
        .map(|variables| variables.filter_map(Result::ok).collect())
        .unwrap_or_default();
    env.extend(std::env::vars());
    env
}
