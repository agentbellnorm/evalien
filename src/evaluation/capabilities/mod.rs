//! What the guest may do beyond computing. Each granted capability becomes a
//! JavaScript global; every call arrives here as a name and JSON arguments,
//! and this is the one place that decides what happens.

mod fetch;
mod sql;

use serde::de::DeserializeOwned;

pub use fetch::{Fetch, is_public};
pub use sql::Sql;

/// The capabilities granted to a sandbox. Nothing is granted by default.
#[derive(Clone, Default)]
pub struct Capabilities {
    sql: Option<Sql>,
    fetch: Option<Fetch>,
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

    pub fn names(&self) -> Vec<String> {
        let granted = [("sql", self.sql.is_some()), ("fetch", self.fetch.is_some())];
        granted
            .into_iter()
            .filter(|(_, granted)| *granted)
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    /// Run capability `name` with JSON `args`. Answers with JSON, or an error
    /// message for the guest.
    pub async fn call(&self, name: &str, args: &str) -> Result<String, String> {
        let value = match (name, &self.sql, &self.fetch) {
            ("sql", Some(sql), _) => sql.execute(parse(args)?).await?,
            ("fetch", _, Some(fetch)) => fetch.request(parse(args)?).await?,
            _ => return Err(format!("no capability named {name:?}")),
        };
        Ok(value.to_string())
    }
}

fn parse<T: DeserializeOwned>(args: &str) -> Result<T, String> {
    serde_json::from_str(args).map_err(|e| format!("invalid arguments: {e}"))
}
