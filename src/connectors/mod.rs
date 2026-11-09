//! The credential gateway: connections to external services, attached to the
//! agent's requests by origin. The agent calls a service as if it were
//! calling it directly; the credentials never enter the sandbox.
//!
//! - API keys are attached as configured.
//! - OAuth connections log in when the agent calls `connect`: the human gets
//!   the sign-in link as a notice, the callback finishes the login, and the
//!   refresh token goes to the [`Vault`]. Access tokens live in memory and are
//!   refreshed ahead of expiry, so attaching them never waits on the network.
//! - BankID connections log in the same way, with an autostart link for the
//!   BankID app. Their session is cookies, kept in the vault and kept alive
//!   while the agent uses it.
//!
//! [`config`] reads what's configured, [`providers`] turns provider names
//! into implementations, and [`Connectors`] routes requests to each
//! connection's session, which runs itself. See docs/connectors.md.

mod api_key;
mod bankid;
mod bankid_session;
mod browser;
mod callback;
pub mod config;
mod context;
mod login;
mod oauth;
mod oauth_session;
pub mod providers;
mod seb;
mod vault;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::Url;

use crate::domain::{
    Attachment, Connecting, ConnectionInfo, ConnectionKind, ConnectionStatus, Gateway, Origin, RequestCounts,
    RequestOutcome, Secret, Trajectory,
};

use api_key::ApiKey;
use bankid_session::BankIdSession;
use context::{Context, lock};
use oauth_session::OAuthSession;
use vault::ConnectionVault;

pub use bankid::{BankIdProvider, BankIdTiming, Progress, find_link as find_bankid_link};
pub use callback::serve as serve_callback;
pub use config::{ConnectionConfig, GatewayConfig, KindConfig};
pub use oauth::{OAuthClient, OAuthError};
pub use oauth_session::OAuthLogins;
pub use providers::{Credentials, Service};
pub use seb::Seb;
pub use vault::{Vault, VaultError, VaultKey};

/// What the sessions need from the rest of the process.
pub struct Deps {
    /// Where sessions keep tokens and cookies. OAuth and BankID need it.
    pub vault: Option<Arc<Vault>>,
    /// For OAuth's token requests. BankID sessions bring their own browser.
    pub http: reqwest::Client,
    /// Where notices for the human go.
    pub log: Arc<Trajectory>,
    pub debug: fn(&str),
}

pub struct Connectors {
    connections: Vec<Connection>,
    /// Which connection serves each origin. Config allows one per origin.
    by_origin: HashMap<Origin, usize>,
}

struct Connection {
    name: String,
    origins: Vec<Origin>,
    skill: Option<&'static str>,
    session: Session,
    requests: Mutex<RequestCounts>,
}

/// Each kind of connection, and the state it keeps.
enum Session {
    ApiKey(ApiKey),
    OAuth(Arc<OAuthSession>),
    BankId(Arc<BankIdSession>),
}

impl Connectors {
    /// Build every connection's session, then start them: restore sessions
    /// from the vault, check or refresh them, and keep them fresh from now on.
    pub async fn start(services: Vec<Service>, deps: Deps) -> Result<Arc<Self>, String> {
        let mut by_origin = HashMap::new();
        let mut connections = Vec::new();
        for (index, service) in services.into_iter().enumerate() {
            for origin in &service.origins {
                by_origin.insert(origin.clone(), index);
            }
            let session = Session::new(&service.name, service.credentials, &deps)?;
            connections.push(Connection {
                name: service.name,
                origins: service.origins,
                skill: service.skill,
                session,
                requests: Mutex::new(RequestCounts::default()),
            });
        }
        for connection in &connections {
            connection.session.start().await;
        }
        Ok(Arc::new(Connectors { connections, by_origin }))
    }

    /// The OAuth logins, for the callback to finish.
    pub fn oauth_logins(&self) -> OAuthLogins {
        OAuthLogins::new(
            self.connections
                .iter()
                .filter_map(|connection| match &connection.session {
                    Session::OAuth(session) => Some(session.clone()),
                    _ => None,
                })
                .collect(),
        )
    }

    fn named(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|connection| connection.name == name)
    }
}

impl Session {
    fn new(name: &str, credentials: Credentials, deps: &Deps) -> Result<Self, String> {
        let context = || {
            let vault = deps
                .vault
                .clone()
                .ok_or_else(|| format!("connection {name} keeps its session in the vault: set VAULT_KEY"))?;
            Ok::<_, String>(Context::new(
                name,
                ConnectionVault::new(vault, name),
                deps.log.clone(),
                deps.debug,
            ))
        };
        Ok(match credentials {
            Credentials::ApiKey(key) => Session::ApiKey(key),
            Credentials::OAuth { client, redirect } => {
                Session::OAuth(OAuthSession::new(context()?, client, deps.http.clone(), redirect))
            }
            Credentials::BankId {
                provider,
                timing,
                keepalive_for,
            } => Session::BankId(BankIdSession::new(context()?, provider, timing, keepalive_for)?),
        })
    }

    async fn start(&self) {
        match self {
            Session::ApiKey(_) => {}
            Session::OAuth(session) => session.start().await,
            Session::BankId(session) => session.start().await,
        }
    }

    fn kind(&self) -> ConnectionKind {
        match self {
            Session::ApiKey(_) => ConnectionKind::ApiKey,
            Session::OAuth(_) => ConnectionKind::OAuth,
            Session::BankId(_) => ConnectionKind::BankId,
        }
    }

    fn status(&self) -> ConnectionStatus {
        match self {
            Session::ApiKey(_) => ConnectionStatus::Connected,
            Session::OAuth(session) => session.status(),
            Session::BankId(session) => session.status(),
        }
    }

    /// The headers for a request to `url`, or why there are none.
    fn attach(&self, url: &Url) -> Result<Vec<(String, Secret)>, ConnectionStatus> {
        match self {
            Session::ApiKey(key) => Ok(key.headers()),
            Session::OAuth(session) => session.attach(),
            Session::BankId(session) => session.attach(url),
        }
    }

    fn observe(&self, response: &crate::domain::ResponseHead) {
        match self {
            Session::ApiKey(_) => {}
            Session::OAuth(session) => session.observe(response),
            Session::BankId(session) => session.observe(response),
        }
    }

    async fn connect(&self, timeout: Duration) -> Result<ConnectionStatus, String> {
        match self {
            Session::ApiKey(_) => Ok(ConnectionStatus::Connected),
            Session::OAuth(session) => session.connect(timeout).await,
            Session::BankId(session) => session.connect(timeout).await,
        }
    }
}

impl Gateway for Connectors {
    fn attach(&self, url: &Url) -> Attachment {
        let Some(&index) = self.by_origin.get(&Origin::of(url)) else {
            return Attachment::Nothing;
        };
        let connection = &self.connections[index];
        match connection.session.attach(url) {
            Ok(headers) => Attachment::Credentials {
                connection: connection.name.clone(),
                headers,
            },
            Err(status) => Attachment::NotConnected {
                connection: connection.name.clone(),
                status,
            },
        }
    }

    fn observe(&self, connection: &str, outcome: RequestOutcome) -> ConnectionStatus {
        let Some(connection) = self.named(connection) else {
            return ConnectionStatus::NotConnected;
        };
        {
            let mut requests = lock(&connection.requests);
            match &outcome {
                RequestOutcome::Response(head) => match head.status {
                    401 | 403 => requests.rejected += 1,
                    status if status >= 500 => requests.server_errors += 1,
                    status if status >= 400 => requests.client_errors += 1,
                    _ => requests.succeeded += 1,
                },
                RequestOutcome::Failed => requests.failed += 1,
            }
        }
        if let RequestOutcome::Response(head) = &outcome {
            connection.session.observe(head);
        }
        connection.session.status()
    }

    fn connections(&self) -> Vec<ConnectionInfo> {
        self.connections
            .iter()
            .map(|connection| ConnectionInfo {
                name: connection.name.clone(),
                kind: connection.session.kind(),
                status: connection.session.status(),
                origins: connection.origins.clone(),
                requests: *lock(&connection.requests),
                skill: connection.skill.is_some(),
            })
            .collect()
    }

    fn skill(&self, connection: &str) -> Option<String> {
        self.named(connection)?.skill.map(str::to_owned)
    }

    fn connect(&self, name: &str, timeout: Duration) -> Connecting<'_> {
        let name = name.to_owned();
        Box::pin(async move {
            let connection = self
                .named(&name)
                .ok_or_else(|| format!("no connection named {name:?}"))?;
            connection.session.connect(timeout).await
        })
    }
}
