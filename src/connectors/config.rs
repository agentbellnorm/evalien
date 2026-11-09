//! The gateway's configuration. Connections come from a TOML file named by
//! CONNECTIONS_FILE; secrets are named there, not written, and each comes from
//! an environment variable, which in the cluster is the Kubernetes Secret.
//!
//! ```toml
//! [connections.example]
//! kind = "api_key"
//! origins = ["https://api.example.com"]
//! header = { name = "Authorization", value = "Bearer {key}" }
//! key = { secret = "EXAMPLE_API_KEY" }
//!
//! [connections.google-calendar]
//! kind = "oauth"
//! provider = "google"
//! origins = ["https://www.googleapis.com"]
//! scopes = ["https://www.googleapis.com/auth/calendar.readonly"]
//! client_id = { secret = "GOOGLE_CLIENT_ID" }
//! client_secret = { secret = "GOOGLE_CLIENT_SECRET" }
//! ```
//!
//! OAuth connections also need VAULT_KEY (where their tokens are kept),
//! CALLBACK_ADDR (where the callback listens) and OAUTH_REDIRECT_URL (how the
//! browser reaches it).

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::Url;
use serde::Deserialize;

use crate::domain::{Origin, Secret};

use super::{
    bankid::BankIdProvider,
    oauth::OAuthClient,
    vault::{VaultError, VaultKey},
};

/// Everything the gateway needs, checked.
#[derive(Default)]
pub struct GatewayConfig {
    pub connections: Vec<ConnectionConfig>,
    pub vault: Option<VaultConfig>,
    pub callback: Option<CallbackConfig>,
}

pub struct VaultConfig {
    pub path: PathBuf,
    pub key: VaultKey,
}

/// Where the OAuth callback listens, and the URL the provider sends the browser to.
pub struct CallbackConfig {
    pub listen: SocketAddr,
    pub redirect: Url,
}

/// One connection, checked and with its secrets filled in.
pub struct ConnectionConfig {
    pub name: String,
    pub origins: Vec<Origin>,
    pub kind: KindConfig,
}

pub enum KindConfig {
    /// A fixed header on every request.
    ApiKey { header: String, value: Secret },
    /// Bearer tokens from an OAuth login, refreshed as needed.
    OAuth { client: Box<OAuthClient> },
    /// Session cookies from a Mobile BankID login, kept alive while in use.
    BankId {
        provider: Box<BankIdProvider>,
        keepalive_for: Duration,
    },
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
    #[serde(rename = "oauth")]
    OAuth {
        provider: String,
        origins: Vec<String>,
        scopes: Vec<String>,
        client_id: SecretRef,
        client_secret: SecretRef,
    },
    #[serde(rename = "bankid")]
    BankId {
        provider: String,
        origins: Vec<String>,
        /// How long to keep the session alive after the agent last used it, e.g. "1h".
        keepalive_for: Option<String>,
    },
}

/// How long a BankID session is kept alive after its last use, unless configured.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(60 * 60);

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

/// The gateway's configuration from the environment. `state_dir` is where the
/// vault goes unless VAULT_PATH says otherwise.
pub fn read(env: &HashMap<String, String>, state_dir: &Path) -> Result<GatewayConfig, String> {
    let set = |name: &str| {
        env.get(name)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    };
    let connections = match set("CONNECTIONS_FILE") {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("CONNECTIONS_FILE {path}: {e}"))?;
            parse(&text, env).map_err(|e| format!("{path}: {e}"))?
        }
        None => Vec::new(),
    };
    let vault = match set("VAULT_KEY") {
        Some(key) => Some(VaultConfig {
            path: set("VAULT_PATH").map_or_else(|| state_dir.join("vault.db"), PathBuf::from),
            key: VaultKey::from_base64(key).map_err(|e: VaultError| e.to_string())?,
        }),
        None => None,
    };
    let callback = match (set("CALLBACK_ADDR"), set("OAUTH_REDIRECT_URL")) {
        (Some(listen), Some(redirect)) => Some(CallbackConfig {
            listen: listen.parse().map_err(|e| format!("CALLBACK_ADDR: {e}"))?,
            redirect: Url::parse(redirect).map_err(|e| format!("OAUTH_REDIRECT_URL: {e}"))?,
        }),
        (None, None) => None,
        _ => return Err("set both CALLBACK_ADDR and OAUTH_REDIRECT_URL, or neither".into()),
    };
    let needs_login = connections
        .iter()
        .any(|connection| matches!(connection.kind, KindConfig::OAuth { .. }));
    let keeps_sessions = connections
        .iter()
        .any(|connection| matches!(connection.kind, KindConfig::BankId { .. }));
    if keeps_sessions && vault.is_none() {
        return Err(
            "BankID connections keep their session in the vault: set VAULT_KEY (openssl rand -base64 32)".into(),
        );
    }
    if needs_login && vault.is_none() {
        return Err("OAuth connections keep tokens in the vault: set VAULT_KEY (openssl rand -base64 32)".into());
    }
    if needs_login && callback.is_none() {
        return Err("OAuth connections need a callback: set CALLBACK_ADDR and OAUTH_REDIRECT_URL".into());
    }
    Ok(GatewayConfig {
        connections,
        vault,
        callback,
    })
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
        let secret = |reference: &SecretRef| {
            env.get(&reference.secret)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| fail(format!("set {}", reference.secret)))
        };
        let (origins, kind) = match spec {
            Spec::ApiKey { origins, header, key } => {
                let header_name = header.name.trim().to_owned();
                let valid =
                    !header_name.is_empty() && header_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                if !valid || RESERVED_HEADERS.contains(&header_name.to_ascii_lowercase().as_str()) {
                    return Err(fail(format!("{header_name:?} can't carry a key")));
                }
                if header.value.matches("{key}").count() != 1 {
                    return Err(fail("the header value must contain {key} once".into()));
                }
                let value = Secret::new(header.value.replace("{key}", &secret(&key)?));
                (
                    origins,
                    KindConfig::ApiKey {
                        header: header_name,
                        value,
                    },
                )
            }
            Spec::OAuth {
                provider,
                origins,
                scopes,
                client_id,
                client_secret,
            } => {
                if scopes.is_empty() {
                    return Err(fail("list the scopes to ask for".into()));
                }
                let client = OAuthClient::for_provider(
                    &provider,
                    secret(&client_id)?,
                    Secret::new(secret(&client_secret)?),
                    scopes,
                )
                .ok_or_else(|| fail(format!("unknown OAuth provider {provider:?}; known: google")))?;
                (
                    origins,
                    KindConfig::OAuth {
                        client: Box::new(client),
                    },
                )
            }
            Spec::BankId {
                provider,
                origins,
                keepalive_for,
            } => {
                let provider = BankIdProvider::for_name(&provider)
                    .ok_or_else(|| fail(format!("unknown BankID provider {provider:?}; known: seb")))?;
                let keepalive_for = match keepalive_for {
                    Some(text) => humantime::parse_duration(&text).map_err(|e| fail(format!("keepalive_for: {e}")))?,
                    None => DEFAULT_KEEPALIVE,
                };
                (
                    origins,
                    KindConfig::BankId {
                        provider: Box::new(provider),
                        keepalive_for,
                    },
                )
            }
        };
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
        connections.push(ConnectionConfig { name, origins, kind });
    }
    Ok(connections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        [
            ("EXAMPLE_KEY", "k-123"),
            ("CLIENT_ID", "id-1"),
            ("CLIENT_SECRET", "cs-1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
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
        let KindConfig::ApiKey { header, value } = &connections[0].kind else {
            panic!("expected an API key")
        };
        assert_eq!((header.as_str(), value.expose()), ("Authorization", "Bearer k-123"));
    }

    #[test]
    fn an_oauth_connection_names_a_known_provider_and_its_client() {
        let text = r#"
            [connections.google-calendar]
            kind = "oauth"
            provider = "google"
            origins = ["https://www.googleapis.com"]
            scopes = ["https://www.googleapis.com/auth/calendar.readonly"]
            client_id = { secret = "CLIENT_ID" }
            client_secret = { secret = "CLIENT_SECRET" }
        "#;
        let connections = parse(text, &env()).unwrap();
        assert!(matches!(connections[0].kind, KindConfig::OAuth { .. }));
        let unknown = parse(&text.replace("\"google\"", "\"myspace\""), &env()).err().unwrap();
        assert!(unknown.contains("unknown OAuth provider"), "{unknown}");
    }

    #[test]
    fn a_bankid_connection_names_its_provider_and_how_long_to_keep_it() {
        let text = |keepalive: &str, provider: &str| {
            format!(
                "[connections.seb]\nkind = \"bankid\"\nprovider = \"{provider}\"\norigins = [\"https://apps.seb.se\"]\n{keepalive}"
            )
        };
        let configured = parse(&text("keepalive_for = \"30m\"", "seb"), &env()).unwrap();
        assert!(
            matches!(configured[0].kind, KindConfig::BankId { keepalive_for, .. } if keepalive_for == Duration::from_secs(1800))
        );
        let default = parse(&text("", "seb"), &env()).unwrap();
        assert!(
            matches!(default[0].kind, KindConfig::BankId { keepalive_for, .. } if keepalive_for == DEFAULT_KEEPALIVE)
        );
        assert!(
            parse(&text("", "nordea"), &env())
                .err()
                .unwrap()
                .contains("unknown BankID provider")
        );
        assert!(
            parse(&text("keepalive_for = \"soon\"", "seb"), &env())
                .err()
                .unwrap()
                .contains("keepalive_for")
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("connections.toml");
        std::fs::write(&file, text("", "seb")).unwrap();
        let mut env = env();
        env.insert("CONNECTIONS_FILE".into(), file.display().to_string());
        assert!(read(&env, dir.path()).err().unwrap().contains("VAULT_KEY"));
    }

    #[test]
    fn oauth_needs_the_vault_and_a_callback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("connections.toml");
        std::fs::write(&file, "[connections.g]\nkind = \"oauth\"\nprovider = \"google\"\norigins = [\"https://www.googleapis.com\"]\nscopes = [\"s\"]\nclient_id = { secret = \"CLIENT_ID\" }\nclient_secret = { secret = \"CLIENT_SECRET\" }\n").unwrap();
        let mut env = env();
        env.insert("CONNECTIONS_FILE".into(), file.display().to_string());
        let error = read(&env, dir.path()).err().unwrap();
        assert!(error.contains("VAULT_KEY"), "{error}");
        env.insert(
            "VAULT_KEY".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        );
        let error = read(&env, dir.path()).err().unwrap();
        assert!(error.contains("CALLBACK_ADDR"), "{error}");
        env.insert("CALLBACK_ADDR".into(), "0.0.0.0:8080".into());
        env.insert(
            "OAUTH_REDIRECT_URL".into(),
            "https://evalien.bushnet.diy/oauth/callback".into(),
        );
        let config = read(&env, dir.path()).unwrap();
        assert_eq!(config.vault.unwrap().path, dir.path().join("vault.db"));
        assert_eq!(config.callback.unwrap().redirect.path(), "/oauth/callback");
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
            let error = parse(&text, &env()).err().unwrap();
            assert!(error.contains(reason), "expected {reason:?}, got {error:?}");
        }
    }
}
