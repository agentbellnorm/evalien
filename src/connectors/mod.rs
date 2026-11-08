//! The credential gateway: connections to external services, attached to the
//! agent's requests by origin. The agent calls a service as if it were
//! calling it directly; the credentials never enter the sandbox.
//!
//! Connections come from config (see [`config`]). Today they're API keys;
//! OAuth and BankID logins come next and keep what they earn in the [`Vault`].
//! See docs/connectors.md.

pub mod config;
mod vault;

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use crate::domain::{
    Attachment, ConnectionInfo, ConnectionKind, ConnectionStatus, Gateway, Origin, RequestCounts, RequestOutcome,
    Secret,
};

pub use config::{ConnectionConfig, KindConfig};
pub use vault::{Vault, VaultError, VaultKey};

pub struct Connectors {
    connections: Vec<Connection>,
    /// Which connection serves each origin. Config allows one per origin.
    by_origin: HashMap<Origin, usize>,
}

struct Connection {
    name: String,
    origins: Vec<Origin>,
    credentials: Credentials,
    requests: Mutex<RequestCounts>,
}

enum Credentials {
    ApiKey { header: String, value: Secret },
}

impl Connectors {
    pub fn new(configs: Vec<ConnectionConfig>) -> Self {
        let mut by_origin = HashMap::new();
        let connections = configs
            .into_iter()
            .enumerate()
            .map(|(index, config)| {
                for origin in &config.origins {
                    by_origin.insert(origin.clone(), index);
                }
                let credentials = match config.kind {
                    KindConfig::ApiKey { header, value } => Credentials::ApiKey { header, value },
                };
                Connection {
                    name: config.name,
                    origins: config.origins,
                    credentials,
                    requests: Mutex::new(RequestCounts::default()),
                }
            })
            .collect();
        Connectors { connections, by_origin }
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    fn named(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|connection| connection.name == name)
    }
}

impl Connection {
    fn kind(&self) -> ConnectionKind {
        match self.credentials {
            Credentials::ApiKey { .. } => ConnectionKind::ApiKey,
        }
    }

    /// An API key has no login to lose: a rejection is counted, and the next
    /// request carries the key again.
    fn status(&self) -> ConnectionStatus {
        match self.credentials {
            Credentials::ApiKey { .. } => ConnectionStatus::Connected,
        }
    }

    fn requests(&self) -> std::sync::MutexGuard<'_, RequestCounts> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Gateway for Connectors {
    fn attach(&self, origin: &Origin) -> Attachment {
        let Some(connection) = self.by_origin.get(origin).map(|&index| &self.connections[index]) else {
            return Attachment::Nothing;
        };
        let headers = match &connection.credentials {
            Credentials::ApiKey { header, value } => vec![(header.clone(), value.clone())],
        };
        Attachment::Credentials {
            connection: connection.name.clone(),
            headers,
        }
    }

    fn observe(&self, connection: &str, outcome: RequestOutcome) -> ConnectionStatus {
        let Some(connection) = self.named(connection) else {
            return ConnectionStatus::Expired;
        };
        {
            let mut requests = connection.requests();
            match outcome {
                RequestOutcome::Status(401 | 403) => requests.rejected += 1,
                RequestOutcome::Status(status) if status >= 500 => requests.server_errors += 1,
                RequestOutcome::Status(status) if status >= 400 => requests.client_errors += 1,
                RequestOutcome::Status(_) => requests.succeeded += 1,
                RequestOutcome::Failed => requests.failed += 1,
            }
        }
        connection.status()
    }

    fn connections(&self) -> Vec<ConnectionInfo> {
        self.connections
            .iter()
            .map(|connection| ConnectionInfo {
                name: connection.name.clone(),
                kind: connection.kind(),
                status: connection.status(),
                origins: connection.origins.clone(),
                requests: *connection.requests(),
            })
            .collect()
    }
}
