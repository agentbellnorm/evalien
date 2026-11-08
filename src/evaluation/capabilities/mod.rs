//! What the guest may do beyond computing. Each granted capability becomes a
//! JavaScript global; every call arrives here as a name and JSON arguments,
//! and this is the one place that decides what happens.

mod fetch;
mod sql;

use std::sync::Arc;

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::domain::Gateway;

pub use fetch::{CONNECTION_HEADER, Fetch, is_public};
pub use sql::Sql;

/// The capabilities granted to a sandbox. Nothing is granted by default.
#[derive(Clone, Default)]
pub struct Capabilities {
    sql: Option<Sql>,
    fetch: Option<Fetch>,
    connections: Option<Arc<dyn Gateway>>,
}

/// What the agent can ask about its connections.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum ConnectionsRequest {
    List,
}

impl Capabilities {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn with_sql(self, sql: Sql) -> Self {
        Capabilities { sql: Some(sql), ..self }
    }

    pub fn with_fetch(self, fetch: Fetch) -> Self {
        Capabilities {
            fetch: Some(fetch),
            ..self
        }
    }

    /// Let the agent see its connections. Credentials stay in the gateway.
    pub fn with_connections(self, gateway: Arc<dyn Gateway>) -> Self {
        Capabilities {
            connections: Some(gateway),
            ..self
        }
    }

    pub fn names(&self) -> Vec<String> {
        let granted = [
            ("sql", self.sql.is_some()),
            ("fetch", self.fetch.is_some()),
            ("connections", self.connections.is_some()),
        ];
        granted
            .into_iter()
            .filter(|(_, granted)| *granted)
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    /// Run capability `name` with JSON `args`. Answers with JSON, or an error
    /// message for the guest.
    pub async fn call(&self, name: &str, args: &str) -> Result<String, String> {
        let value = match name {
            "sql" if let Some(sql) = &self.sql => sql.execute(parse(args)?).await?,
            "fetch" if let Some(fetch) = &self.fetch => fetch.request(parse(args)?).await?,
            "connections" if let Some(gateway) = &self.connections => match parse(args)? {
                ConnectionsRequest::List => list(gateway.as_ref()),
            },
            _ => return Err(format!("no capability named {name:?}")),
        };
        Ok(value.to_string())
    }
}

fn list(gateway: &dyn Gateway) -> Value {
    let connections: Vec<Value> = gateway
        .connections()
        .into_iter()
        .map(|connection| {
            let origins: Vec<&str> = connection.origins.iter().map(|origin| origin.as_str()).collect();
            json!({
                "name": connection.name,
                "kind": connection.kind.as_str(),
                "status": connection.status.as_str(),
                "origins": origins,
            })
        })
        .collect();
    Value::Array(connections)
}

fn parse<T: DeserializeOwned>(args: &str) -> Result<T, String> {
    serde_json::from_str(args).map_err(|e| format!("invalid arguments: {e}"))
}
