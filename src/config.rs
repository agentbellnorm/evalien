//! Everything the agent reads from the environment.

use std::{collections::HashMap, path::PathBuf};

use crate::inference::{self, ConfigError, InferenceConfig};

pub struct Config {
    pub db_path: PathBuf,
    pub inference: InferenceConfig,
}

pub fn read(env: &HashMap<String, String>) -> Result<Config, ConfigError> {
    Ok(Config {
        db_path: db_path(env),
        inference: inference::config::read(env)?,
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
