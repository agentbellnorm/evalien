//! Connections, from a TOML file named by CONNECTIONS_FILE. Secrets are
//! named, not written: each comes from an environment variable, which in the
//! cluster is the Kubernetes Secret.
//!
//! ```toml
//! [connections.example]
//! kind = "api_key"
//! origins = ["https://api.example.com"]
//! header = { name = "Authorization", value = "Bearer {key}" }
//! key = { secret = "EXAMPLE_API_KEY" }
//! ```

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use crate::domain::{Origin, Secret};

/// One connection, checked and with its secrets filled in.
#[derive(Debug, PartialEq)]
pub struct ConnectionConfig {
    pub name: String,
    pub origins: Vec<Origin>,
    pub kind: KindConfig,
}

#[derive(Debug, PartialEq)]
pub enum KindConfig {
    /// A fixed header on every request.
    ApiKey { header: String, value: Secret },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    connections: BTreeMap<String, Spec>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Spec {
    ApiKey {
        origins: Vec<String>,
        header: HeaderSpec,
        key: SecretRef,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderSpec {
    name: String,
    /// Where the key goes, as `{key}`.
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretRef {
    secret: String,
}

/// Headers the gateway may never set: they'd change how the request is
/// routed or framed rather than who's asking.
const RESERVED_HEADERS: [&str; 6] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
];

/// The connections in the file named by CONNECTIONS_FILE, or none.
pub fn read(env: &HashMap<String, String>) -> Result<Vec<ConnectionConfig>, String> {
    let Some(path) = env.get("CONNECTIONS_FILE").filter(|path| !path.is_empty()) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("CONNECTIONS_FILE {path}: {e}"))?;
    parse(&text, env).map_err(|e| format!("{path}: {e}"))
}

pub fn parse(text: &str, env: &HashMap<String, String>) -> Result<Vec<ConnectionConfig>, String> {
    let file: File = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut claimed: HashMap<Origin, String> = HashMap::new();
    let mut connections = Vec::new();
    for (name, spec) in file.connections {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "connection {name:?}: names are lowercase letters, digits and dashes"
            ));
        }
        let fail = |message: String| format!("connection {name}: {message}");
        let Spec::ApiKey { origins, header, key } = spec;
        let origins = origins
            .iter()
            .map(|origin| Origin::parse(origin))
            .collect::<Result<Vec<_>, _>>()
            .map_err(fail)?;
        if origins.is_empty() {
            return Err(fail("list at least one origin".into()));
        }
        for origin in &origins {
            if let Some(other) = claimed.insert(origin.clone(), name.clone()) {
                return Err(fail(format!("{origin} already belongs to connection {other}")));
            }
        }
        let header_name = header.name.trim();
        let valid = !header_name.is_empty() && header_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !valid || RESERVED_HEADERS.contains(&header_name.to_ascii_lowercase().as_str()) {
            return Err(fail(format!("{header_name:?} can't carry a key")));
        }
        if header.value.matches("{key}").count() != 1 {
            return Err(fail("the header value must contain {key} once".into()));
        }
        let secret = env
            .get(&key.secret)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| fail(format!("set {}", key.secret)))?;
        let value = Secret::new(header.value.replace("{key}", secret));
        connections.push(ConnectionConfig {
            name,
            origins,
            kind: KindConfig::ApiKey {
                header: header_name.to_owned(),
                value,
            },
        });
    }
    Ok(connections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        [("EXAMPLE_KEY".to_owned(), "k-123".to_owned())].into_iter().collect()
    }

    #[test]
    fn an_api_key_connection_gets_its_secret_from_the_environment() {
        let text = r#"
            [connections.example]
            kind = "api_key"
            origins = ["https://api.example.com", "https://upload.example.com:8443"]
            header = { name = "Authorization", value = "Bearer {key}" }
            key = { secret = "EXAMPLE_KEY" }
        "#;
        let connections = parse(text, &env()).unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].origins[1].as_str(), "https://upload.example.com:8443");
        let KindConfig::ApiKey { header, value } = &connections[0].kind;
        assert_eq!((header.as_str(), value.expose()), ("Authorization", "Bearer k-123"));
        assert_eq!(
            format!("{connections:?}").matches("k-123").count(),
            0,
            "secrets never print"
        );
    }

    #[test]
    fn mistakes_are_refused_with_a_reason() {
        let connection = |name: &str, origins: &str, header: &str, value: &str, secret: &str| {
            format!(
                "[connections.{name}]\nkind = \"api_key\"\norigins = [{origins}]\n\
                 header = {{ name = \"{header}\", value = \"{value}\" }}\nkey = {{ secret = \"{secret}\" }}\n"
            )
        };
        let good = connection("a", "\"https://a.example\"", "X-Api-Key", "{key}", "EXAMPLE_KEY");
        assert!(parse(&good, &env()).is_ok());
        for (text, reason) in [
            (
                connection("a", "\"http://a.example\"", "X-Api-Key", "{key}", "EXAMPLE_KEY"),
                "only go over https",
            ),
            (
                connection("a", "\"https://a.example/v1\"", "X-Api-Key", "{key}", "EXAMPLE_KEY"),
                "just https://host",
            ),
            (
                connection("a", "", "X-Api-Key", "{key}", "EXAMPLE_KEY"),
                "at least one origin",
            ),
            (
                connection("a", "\"https://a.example\"", "Host", "{key}", "EXAMPLE_KEY"),
                "can't carry a key",
            ),
            (
                connection("a", "\"https://a.example\"", "X-Api-Key", "Bearer", "EXAMPLE_KEY"),
                "contain {key} once",
            ),
            (
                connection("a", "\"https://a.example\"", "X-Api-Key", "{key}", "MISSING"),
                "set MISSING",
            ),
            (
                connection("Bad_Name", "\"https://a.example\"", "X-Api-Key", "{key}", "EXAMPLE_KEY"),
                "lowercase letters",
            ),
            (
                good.clone() + &connection("b", "\"https://a.example\"", "X-Api-Key", "{key}", "EXAMPLE_KEY"),
                "already belongs to connection a",
            ),
            (
                "[connections.a]\nkind = \"carrier_pigeon\"\n".to_owned(),
                "unknown variant",
            ),
        ] {
            let error = parse(&text, &env()).unwrap_err();
            assert!(error.contains(reason), "expected {reason:?}, got {error:?}");
        }
    }
}
