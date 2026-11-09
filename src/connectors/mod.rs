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
//! See docs/connectors.md.

mod bankid;
mod bankid_session;
mod browser;
mod callback;
pub mod config;
mod oauth;
mod seb;
mod vault;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::{Duration, Instant},
};

use reqwest::Url;
use tokio::sync::{Notify, watch};

use crate::domain::{
    Attachment, Connecting, ConnectionInfo, ConnectionKind, ConnectionStatus, Event, Gateway, Origin, RequestCounts,
    RequestOutcome, Secret, Trajectory,
};

use bankid_session::BankId;

pub use bankid::{BankIdProvider, Progress};
pub use callback::serve as serve_callback;
pub use config::{ConnectionConfig, GatewayConfig, KindConfig};
pub use oauth::{OAuthClient, OAuthError};
pub use seb::Seb;
pub use vault::{Vault, VaultError, VaultKey};

/// Refresh access tokens this long before they expire.
const REFRESH_AHEAD: Duration = Duration::from_secs(5 * 60);
/// Wait this long before retrying a refresh that failed for a temporary reason.
const REFRESH_RETRY: Duration = Duration::from_secs(30);
/// The vault item holding an OAuth connection's refresh token.
const REFRESH_TOKEN: &str = "refresh_token";

pub struct Connectors {
    /// Itself, for spawning work that outlives a call, like a BankID login.
    this: Weak<Connectors>,
    connections: Vec<Connection>,
    /// Which connection serves each origin. Config allows one per origin.
    by_origin: HashMap<Origin, usize>,
    vault: Option<Vault>,
    log: Arc<Trajectory>,
    http: reqwest::Client,
    redirect: Option<Url>,
    debug: fn(&str),
}

struct Connection {
    name: String,
    origins: Vec<Origin>,
    credentials: Credentials,
    requests: Mutex<RequestCounts>,
}

enum Credentials {
    ApiKey { header: String, value: Secret },
    OAuth(Box<OAuth>),
    BankId(Box<BankId>),
}

struct OAuth {
    client: OAuthClient,
    session: Mutex<Session>,
    /// Asks the refresher to refresh now.
    wake: Notify,
}

struct Session {
    status: ConnectionStatus,
    access: Option<AccessToken>,
    login: Option<Login>,
}

struct AccessToken {
    token: Secret,
    expires: Instant,
}

/// What everyone waiting on a login watches: set once it finishes.
type Waiting = watch::Receiver<Option<Result<(), String>>>;

/// A login waiting for the human.
struct Login {
    state: String,
    verifier: Secret,
    /// The status to return to if the login doesn't finish.
    before: ConnectionStatus,
    /// Set once the login finishes; everyone in `connect` waits on it.
    done: watch::Sender<Option<Result<(), String>>>,
}

impl Connectors {
    pub fn new(config: GatewayConfig, log: Arc<Trajectory>, debug: fn(&str)) -> Result<Arc<Self>, String> {
        let vault = match config.vault {
            Some(vault) => Some(Vault::open(&vault.path, &vault.key).map_err(|e| e.to_string())?),
            None => None,
        };
        let http = reqwest::Client::builder()
            .user_agent("evalien")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let mut by_origin = HashMap::new();
        let mut connections = Vec::new();
        for (index, config) in config.connections.into_iter().enumerate() {
            for origin in &config.origins {
                by_origin.insert(origin.clone(), index);
            }
            let credentials = match config.kind {
                KindConfig::ApiKey { header, value } => Credentials::ApiKey { header, value },
                KindConfig::OAuth { client } => Credentials::OAuth(Box::new(OAuth {
                    client: *client,
                    session: Mutex::new(Session {
                        status: ConnectionStatus::NotConnected,
                        access: None,
                        login: None,
                    }),
                    wake: Notify::new(),
                })),
                KindConfig::BankId {
                    provider,
                    keepalive_for,
                } => Credentials::BankId(Box::new(BankId::new(*provider, keepalive_for)?)),
            };
            connections.push(Connection {
                name: config.name,
                origins: config.origins,
                credentials,
                requests: Mutex::new(RequestCounts::default()),
            });
        }
        let redirect = config.callback.map(|callback| callback.redirect);
        Ok(Arc::new_cyclic(|this| Connectors {
            this: this.clone(),
            connections,
            by_origin,
            vault,
            log,
            http,
            redirect,
            debug,
        }))
    }

    /// Restore sessions from the vault, check or refresh them, and keep them
    /// fresh from now on.
    pub async fn start(self: &Arc<Self>) {
        for (index, connection) in self.connections.iter().enumerate() {
            match &connection.credentials {
                Credentials::ApiKey { .. } => {}
                Credentials::OAuth(oauth) => {
                    if self.refresh_token(connection).is_some() {
                        lock(&oauth.session).status = ConnectionStatus::Connected;
                        self.refresh(connection, oauth).await;
                    }
                    tokio::spawn(self.clone().keep_fresh(index));
                }
                Credentials::BankId(bankid) => {
                    self.restore_bankid(connection, bankid).await;
                    tokio::spawn(self.clone().keep_bankid_alive(index));
                }
            }
        }
    }

    /// Finish the login that issued `state`, with the code the provider sent
    /// the browser back with. Returns the connection's name.
    pub async fn complete(&self, state: &str, code: &str) -> Result<String, String> {
        let (connection, oauth, login) = self.take_login(state)?;
        let redirect = self.redirect.as_ref().ok_or("there's no OAuth callback configured")?;
        let tokens = match oauth.client.exchange(&self.http, redirect, code, &login.verifier).await {
            Ok(tokens) => tokens,
            Err(error) => return Err(self.fail_login(connection, oauth, login, &error.to_string())),
        };
        let Some(refresh) = &tokens.refresh else {
            return Err(self.fail_login(connection, oauth, login, "the provider sent no refresh token"));
        };
        if let Err(error) = self.vault().and_then(|vault| {
            vault
                .put(&connection.name, REFRESH_TOKEN, refresh)
                .map_err(|e| e.to_string())
        }) {
            return Err(self.fail_login(connection, oauth, login, &error));
        }
        {
            let mut session = lock(&oauth.session);
            session.status = ConnectionStatus::Connected;
            session.access = Some(AccessToken {
                token: tokens.access,
                expires: Instant::now() + tokens.expires_in,
            });
        }
        oauth.wake.notify_one();
        login.done.send_replace(Some(Ok(())));
        self.notice(format!("{} is connected.", connection.name));
        Ok(connection.name.clone())
    }

    /// The human or the provider turned the login down.
    pub fn refuse(&self, state: &str, reason: &str) -> Result<String, String> {
        let (connection, oauth, login) = self.take_login(state)?;
        Err(self.fail_login(connection, oauth, login, reason))
    }

    fn take_login(&self, state: &str) -> Result<(&Connection, &OAuth, Login), String> {
        for connection in &self.connections {
            let Credentials::OAuth(oauth) = &connection.credentials else {
                continue;
            };
            let mut session = lock(&oauth.session);
            if session.login.as_ref().is_some_and(|login| login.state == state) {
                let login = session.login.take().ok_or("the login just finished")?;
                return Ok((connection, oauth, login));
            }
        }
        Err("this sign-in link has expired or was already used; ask the agent to connect again".into())
    }

    fn fail_login(&self, connection: &Connection, oauth: &OAuth, login: Login, reason: &str) -> String {
        lock(&oauth.session).status = login.before;
        let message = format!("Signing in to {} failed: {reason}", connection.name);
        login.done.send_replace(Some(Err(message.clone())));
        self.notice(message.clone());
        message
    }

    /// Refresh one connection's access tokens whenever they're due, for as
    /// long as the process runs.
    async fn keep_fresh(self: Arc<Self>, index: usize) {
        let connection = &self.connections[index];
        let Credentials::OAuth(oauth) = &connection.credentials else {
            return;
        };
        loop {
            let due = {
                let session = lock(&oauth.session);
                match (&session.status, &session.access) {
                    (ConnectionStatus::Connected, Some(access)) => Some(
                        access
                            .expires
                            .saturating_duration_since(Instant::now())
                            .saturating_sub(REFRESH_AHEAD),
                    ),
                    (ConnectionStatus::Connected, None) => Some(Duration::ZERO),
                    _ => None,
                }
            };
            match due {
                Some(wait) => {
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        () = oauth.wake.notified() => {}
                    }
                }
                None => oauth.wake.notified().await,
            }
            if !self.refresh(connection, oauth).await {
                tokio::time::sleep(REFRESH_RETRY).await;
            }
        }
    }

    /// Get a new access token. Returns false if it failed for a temporary reason.
    async fn refresh(&self, connection: &Connection, oauth: &OAuth) -> bool {
        if lock(&oauth.session).status != ConnectionStatus::Connected {
            return true;
        }
        let Some(refresh) = self.refresh_token(connection) else {
            lock(&oauth.session).status = ConnectionStatus::NotConnected;
            return true;
        };
        match oauth.client.refresh(&self.http, &refresh).await {
            Ok(tokens) => {
                if let Some(rotated) = &tokens.refresh
                    && let Err(error) = self.vault().and_then(|v| {
                        v.put(&connection.name, REFRESH_TOKEN, rotated)
                            .map_err(|e| e.to_string())
                    })
                {
                    (self.debug)(&format!(
                        "connectors: {}: keeping the rotated refresh token failed: {error}",
                        connection.name
                    ));
                }
                lock(&oauth.session).access = Some(AccessToken {
                    token: tokens.access,
                    expires: Instant::now() + tokens.expires_in,
                });
                true
            }
            Err(OAuthError::Refused(reason)) => {
                if let Ok(vault) = self.vault() {
                    let _ = vault.delete(&connection.name, REFRESH_TOKEN);
                }
                {
                    let mut session = lock(&oauth.session);
                    session.status = ConnectionStatus::Expired;
                    session.access = None;
                }
                self.notice(format!("{} needs signing in again ({reason}).", connection.name));
                true
            }
            Err(OAuthError::Failed(reason)) => {
                (self.debug)(&format!(
                    "connectors: refreshing {} failed, retrying: {reason}",
                    connection.name
                ));
                false
            }
        }
    }

    fn refresh_token(&self, connection: &Connection) -> Option<Secret> {
        let vault = self.vault().ok()?;
        match vault.get(&connection.name, REFRESH_TOKEN) {
            Ok(token) => token,
            Err(error) => {
                (self.debug)(&format!("connectors: {}: {error}", connection.name));
                None
            }
        }
    }

    fn vault(&self) -> Result<&Vault, String> {
        self.vault
            .as_ref()
            .ok_or_else(|| "there's no vault: set VAULT_KEY".to_owned())
    }

    fn notice(&self, text: String) {
        if let Err(error) = self.log.append(Event::Notice { text }) {
            (self.debug)(&format!("connectors: notice not recorded: {error}"));
        }
    }

    fn named(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|connection| connection.name == name)
    }

    /// Start a login, or join the one in progress. Returns the login's
    /// `state` and what to wait on, or `None` if already connected.
    fn begin_login(&self, connection: &Connection, oauth: &OAuth) -> Result<Option<(String, Waiting)>, String> {
        let mut session = lock(&oauth.session);
        if session.status == ConnectionStatus::Connected {
            return Ok(None);
        }
        if let Some(login) = &session.login {
            return Ok(Some((login.state.clone(), login.done.subscribe())));
        }
        let redirect = self.redirect.as_ref().ok_or("there's no OAuth callback configured")?;
        let state = oauth::random_token().map_err(|e| e.to_string())?;
        let pkce = oauth::Pkce::new().map_err(|e| e.to_string())?;
        let url = oauth.client.authorization_url(redirect, &state, &pkce.challenge);
        let (done, waiting) = watch::channel(None);
        session.login = Some(Login {
            state: state.clone(),
            verifier: pkce.verifier,
            before: session.status,
            done,
        });
        session.status = ConnectionStatus::LoggingIn;
        drop(session);
        self.notice(format!("Sign in to {} so the agent can use it: {url}", connection.name));
        Ok(Some((state, waiting)))
    }

    /// Give up on a login that took too long, if it's still the same one.
    fn abandon_login(&self, oauth: &OAuth, state: &str) {
        let mut session = lock(&oauth.session);
        if session.login.as_ref().is_some_and(|login| login.state == state)
            && let Some(login) = session.login.take()
        {
            session.status = login.before;
        }
    }
}

impl Connection {
    /// How to use the service, if its provider says.
    fn skill(&self) -> Option<&'static str> {
        match &self.credentials {
            Credentials::BankId(bankid) => bankid.skill(),
            Credentials::ApiKey { .. } | Credentials::OAuth(_) => None,
        }
    }

    fn kind(&self) -> ConnectionKind {
        match self.credentials {
            Credentials::ApiKey { .. } => ConnectionKind::ApiKey,
            Credentials::OAuth(_) => ConnectionKind::OAuth,
            Credentials::BankId(_) => ConnectionKind::BankId,
        }
    }

    /// An API key has no login to lose: a rejection is counted, and the next
    /// request carries the key again.
    fn status(&self) -> ConnectionStatus {
        match &self.credentials {
            Credentials::ApiKey { .. } => ConnectionStatus::Connected,
            Credentials::OAuth(oauth) => lock(&oauth.session).status,
            Credentials::BankId(bankid) => lock(&bankid.session).status,
        }
    }
}

impl Gateway for Connectors {
    fn attach(&self, url: &Url) -> Attachment {
        let Some(connection) = self
            .by_origin
            .get(&Origin::of(url))
            .map(|&index| &self.connections[index])
        else {
            return Attachment::Nothing;
        };
        let name = connection.name.clone();
        match &connection.credentials {
            Credentials::ApiKey { header, value } => Attachment::Credentials {
                connection: name,
                headers: vec![(header.clone(), value.clone())],
            },
            Credentials::OAuth(oauth) => {
                let session = lock(&oauth.session);
                match &session.access {
                    Some(access)
                        if session.status == ConnectionStatus::Connected && access.expires > Instant::now() =>
                    {
                        let bearer = Secret::new(format!("Bearer {}", access.token.expose()));
                        Attachment::Credentials {
                            connection: name,
                            headers: vec![("Authorization".into(), bearer)],
                        }
                    }
                    _ => {
                        if session.status == ConnectionStatus::Connected {
                            oauth.wake.notify_one();
                        }
                        Attachment::NotConnected {
                            connection: name,
                            status: session.status,
                        }
                    }
                }
            }
            Credentials::BankId(bankid) => {
                let mut session = lock(&bankid.session);
                if session.status != ConnectionStatus::Connected {
                    return Attachment::NotConnected {
                        connection: name,
                        status: session.status,
                    };
                }
                session.last_used = Instant::now();
                let headers = bankid
                    .browser
                    .cookie_header(url)
                    .map(|cookies| ("Cookie".to_owned(), cookies));
                Attachment::Credentials {
                    connection: name,
                    headers: headers.into_iter().collect(),
                }
            }
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
            match &connection.credentials {
                // A rejected token may just be stale: refresh now rather than at expiry.
                Credentials::OAuth(oauth) if head.status == 401 => oauth.wake.notify_one(),
                Credentials::BankId(bankid) => self.observe_bankid(connection, bankid, head),
                _ => {}
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
                requests: *lock(&connection.requests),
                skill: connection.skill().is_some(),
            })
            .collect()
    }

    fn skill(&self, connection: &str) -> Option<String> {
        self.named(connection)?.skill().map(str::to_owned)
    }

    fn connect(&self, name: &str, timeout: Duration) -> Connecting<'_> {
        let name = name.to_owned();
        Box::pin(async move {
            let index = self
                .connections
                .iter()
                .position(|connection| connection.name == name)
                .ok_or_else(|| format!("no connection named {name:?}"))?;
            let connection = &self.connections[index];
            if let Credentials::BankId(_) = &connection.credentials {
                let Some(waiting) = self.begin_bankid(index)? else {
                    return Ok(ConnectionStatus::Connected);
                };
                return wait_for_login(&name, waiting, timeout).await;
            }
            let Credentials::OAuth(oauth) = &connection.credentials else {
                return Ok(connection.status());
            };
            let Some((state, mut waiting)) = self.begin_login(connection, oauth)? else {
                return Ok(ConnectionStatus::Connected);
            };
            let finished = tokio::time::timeout(timeout, waiting.wait_for(Option::is_some))
                .await
                .map(|result| result.map(|outcome| outcome.clone()));
            match finished {
                Ok(Ok(Some(Ok(())))) => Ok(ConnectionStatus::Connected),
                Ok(Ok(Some(Err(message)))) => Err(message),
                Ok(Ok(None)) => Err("the login ended without a result".into()),
                Ok(Err(_)) => Err("the login was dropped".into()),
                Err(_) => {
                    self.abandon_login(oauth, &state);
                    Err(format!("{name}: no sign-in within {}s", timeout.as_secs()))
                }
            }
        })
    }
}

/// Wait for a login to finish. A BankID login keeps running after a timeout
/// here, so a later `connect` can still catch it.
async fn wait_for_login(name: &str, mut waiting: Waiting, timeout: Duration) -> Result<ConnectionStatus, String> {
    let finished = tokio::time::timeout(timeout, waiting.wait_for(Option::is_some))
        .await
        .map(|result| result.map(|outcome| outcome.clone()));
    match finished {
        Ok(Ok(Some(Ok(())))) => Ok(ConnectionStatus::Connected),
        Ok(Ok(Some(Err(message)))) => Err(message),
        Ok(Ok(None)) => Err("the login ended without a result".into()),
        Ok(Err(_)) => Err("the login was dropped".into()),
        Err(_) => Err(format!("{name}: no sign-in within {}s", timeout.as_secs())),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each critical section is a few field updates, so a panic elsewhere
    // can't leave the state half-changed.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
