//! An OAuth connection's session: the login through the browser, the access
//! token in memory, and the refresh token in the vault, refreshed ahead of
//! expiry so attaching a token never waits on the network.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::Url;
use tokio::{sync::Notify, time::Instant};

use crate::domain::{ConnectionStatus, ResponseHead, Secret};

use super::{
    context::{Context, lock},
    login::{self, Login, Waiting},
    oauth::{self, OAuthClient, OAuthError},
};

/// Refresh access tokens this long before they expire.
const REFRESH_AHEAD: Duration = Duration::from_secs(5 * 60);
/// Wait this long before retrying a refresh that failed for a temporary reason.
const REFRESH_RETRY: Duration = Duration::from_secs(30);
/// The vault item holding the refresh token.
const REFRESH_TOKEN: &str = "refresh_token";

pub struct OAuthSession {
    context: Context,
    client: OAuthClient,
    http: reqwest::Client,
    /// Where the provider sends the browser back to: the callback.
    redirect: Url,
    state: Mutex<State>,
    /// Asks the refresher to refresh now.
    wake: Notify,
}

struct State {
    status: ConnectionStatus,
    access: Option<AccessToken>,
    login: Option<Pending>,
}

struct AccessToken {
    token: Secret,
    expires: Instant,
}

/// A login waiting for the human to come back through the callback.
struct Pending {
    state: String,
    verifier: Secret,
    /// The status to return to if the login doesn't finish.
    before: ConnectionStatus,
    login: Login,
}

impl OAuthSession {
    pub fn new(context: Context, client: OAuthClient, http: reqwest::Client, redirect: Url) -> Arc<Self> {
        Arc::new(OAuthSession {
            context,
            client,
            http,
            redirect,
            state: Mutex::new(State {
                status: ConnectionStatus::NotConnected,
                access: None,
                login: None,
            }),
            wake: Notify::new(),
        })
    }

    /// Refresh from the vault's token, if there is one, and keep refreshing
    /// for as long as the process runs.
    pub async fn start(self: &Arc<Self>) {
        if self.refresh_token().is_some() {
            lock(&self.state).status = ConnectionStatus::Connected;
            self.refresh().await;
        }
        tokio::spawn(self.clone().keep_fresh());
    }

    pub fn status(&self) -> ConnectionStatus {
        lock(&self.state).status
    }

    /// The bearer token, or why there isn't one.
    pub fn attach(&self) -> Result<Vec<(String, Secret)>, ConnectionStatus> {
        let state = lock(&self.state);
        match &state.access {
            Some(access) if state.status == ConnectionStatus::Connected && access.expires > Instant::now() => {
                let bearer = Secret::new(format!("Bearer {}", access.token.expose()));
                Ok(vec![("Authorization".into(), bearer)])
            }
            _ => {
                if state.status == ConnectionStatus::Connected {
                    self.wake.notify_one();
                }
                Err(state.status)
            }
        }
    }

    pub fn observe(&self, response: &ResponseHead) {
        // A rejected token may just be stale: refresh now rather than at expiry.
        if response.status == 401 {
            self.wake.notify_one();
        }
    }

    /// Log in, or join the login in progress. The login is abandoned once
    /// nobody is waiting for it any more.
    pub async fn connect(&self, timeout: Duration) -> Result<ConnectionStatus, String> {
        let Some((state, waiting)) = self.begin()? else {
            return Ok(ConnectionStatus::Connected);
        };
        let waited = waiting.wait(timeout).await;
        if waited.is_none() {
            self.abandon_if_unawaited(&state);
        }
        login::answer(&self.context.name, waited, timeout)
    }

    /// Start a login, or join the one in progress. Returns the login's
    /// `state` and what to wait on, or `None` if already connected.
    fn begin(&self) -> Result<Option<(String, Waiting)>, String> {
        let mut state = lock(&self.state);
        if state.status == ConnectionStatus::Connected {
            return Ok(None);
        }
        if let Some(pending) = &state.login {
            return Ok(Some((pending.state.clone(), pending.login.join())));
        }
        let token = oauth::random_token().map_err(|e| e.to_string())?;
        let pkce = oauth::Pkce::new().map_err(|e| e.to_string())?;
        let url = self.client.authorization_url(&self.redirect, &token, &pkce.challenge);
        let (login, waiting) = Login::new();
        state.login = Some(Pending {
            state: token.clone(),
            verifier: pkce.verifier,
            before: state.status,
            login,
        });
        state.status = ConnectionStatus::LoggingIn;
        drop(state);
        self.context.notice(format!(
            "Sign in to {} so the agent can use it: {url}",
            self.context.name
        ));
        Ok(Some((token, waiting)))
    }

    fn abandon_if_unawaited(&self, token: &str) {
        let mut state = lock(&self.state);
        let abandoned = state
            .login
            .as_ref()
            .is_some_and(|pending| pending.state == token && !pending.login.is_awaited());
        if abandoned && let Some(pending) = state.login.take() {
            state.status = pending.before;
        }
    }

    fn take_login(&self, token: &str) -> Option<Pending> {
        let mut state = lock(&self.state);
        if state.login.as_ref().is_some_and(|pending| pending.state == token) {
            state.login.take()
        } else {
            None
        }
    }

    /// Exchange the code the provider sent the browser back with.
    async fn finish(&self, pending: Pending, code: &str) -> Result<String, String> {
        let tokens = match self
            .client
            .exchange(&self.http, &self.redirect, code, &pending.verifier)
            .await
        {
            Ok(tokens) => tokens,
            Err(error) => return Err(self.fail(pending, &error.to_string())),
        };
        let Some(refresh) = &tokens.refresh else {
            return Err(self.fail(pending, "the provider sent no refresh token"));
        };
        if let Err(error) = self.context.vault.put(REFRESH_TOKEN, refresh) {
            return Err(self.fail(pending, &error.to_string()));
        }
        {
            let mut state = lock(&self.state);
            state.status = ConnectionStatus::Connected;
            state.access = Some(AccessToken {
                token: tokens.access,
                expires: Instant::now() + tokens.expires_in,
            });
        }
        self.wake.notify_one();
        pending.login.finish(Ok(()));
        self.context.notice(format!("{} is connected.", self.context.name));
        Ok(self.context.name.clone())
    }

    fn fail(&self, pending: Pending, reason: &str) -> String {
        lock(&self.state).status = pending.before;
        let message = format!("Signing in to {} failed: {reason}", self.context.name);
        pending.login.finish(Err(message.clone()));
        self.context.notice(message.clone());
        message
    }

    /// Refresh the access token whenever it's due.
    async fn keep_fresh(self: Arc<Self>) {
        loop {
            let due = {
                let state = lock(&self.state);
                match (&state.status, &state.access) {
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
                        () = self.wake.notified() => {}
                    }
                }
                None => self.wake.notified().await,
            }
            if !self.refresh().await {
                tokio::time::sleep(REFRESH_RETRY).await;
            }
        }
    }

    /// Get a new access token. Returns false if it failed for a temporary reason.
    async fn refresh(&self) -> bool {
        if self.status() != ConnectionStatus::Connected {
            return true;
        }
        let Some(refresh) = self.refresh_token() else {
            lock(&self.state).status = ConnectionStatus::NotConnected;
            return true;
        };
        match self.client.refresh(&self.http, &refresh).await {
            Ok(tokens) => {
                if let Some(rotated) = &tokens.refresh
                    && let Err(error) = self.context.vault.put(REFRESH_TOKEN, rotated)
                {
                    self.context
                        .debug(&format!("keeping the rotated refresh token failed: {error}"));
                }
                lock(&self.state).access = Some(AccessToken {
                    token: tokens.access,
                    expires: Instant::now() + tokens.expires_in,
                });
                true
            }
            Err(OAuthError::Refused(reason)) => {
                let _ = self.context.vault.delete(REFRESH_TOKEN);
                {
                    let mut state = lock(&self.state);
                    state.status = ConnectionStatus::Expired;
                    state.access = None;
                }
                self.context
                    .notice(format!("{} needs signing in again ({reason}).", self.context.name));
                true
            }
            Err(OAuthError::Failed(reason)) => {
                self.context.debug(&format!("refreshing failed, retrying: {reason}"));
                false
            }
        }
    }

    fn refresh_token(&self) -> Option<Secret> {
        match self.context.vault.get(REFRESH_TOKEN) {
            Ok(token) => token,
            Err(error) => {
                self.context.debug(&error.to_string());
                None
            }
        }
    }
}

/// The logins the callback can finish, found by the `state` they issued.
#[derive(Clone, Default)]
pub struct OAuthLogins(Vec<Arc<OAuthSession>>);

impl OAuthLogins {
    pub fn new(sessions: Vec<Arc<OAuthSession>>) -> Self {
        OAuthLogins(sessions)
    }

    /// Finish the login that issued `state` with the provider's code.
    /// Returns the connection's name.
    pub async fn complete(&self, state: &str, code: &str) -> Result<String, String> {
        let (session, pending) = self.take(state)?;
        session.finish(pending, code).await
    }

    /// The human or the provider turned the login down.
    pub fn refuse(&self, state: &str, reason: &str) -> Result<String, String> {
        let (session, pending) = self.take(state)?;
        Err(session.fail(pending, reason))
    }

    fn take(&self, state: &str) -> Result<(&OAuthSession, Pending), String> {
        self.0
            .iter()
            .find_map(|session| session.take_login(state).map(|pending| (session.as_ref(), pending)))
            .ok_or_else(|| "this sign-in link has expired or was already used; ask the agent to connect again".into())
    }
}
