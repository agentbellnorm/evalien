//! A BankID connection's session: logging in, keeping the session's cookies,
//! keeping it alive while the agent uses it, and noticing when it ends.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::Url;
use tokio::{sync::Notify, time::Instant};

use crate::domain::{ConnectionStatus, ResponseHead, Secret};

use super::{
    bankid::{self, BankIdProvider, BankIdTiming, Progress},
    browser::Browser,
    context::{Context, lock},
    login::{self, Login, Waiting},
};

/// The vault item holding the session's cookies.
const COOKIES: &str = "cookies";

pub struct BankIdSession {
    context: Context,
    provider: BankIdProvider,
    timing: BankIdTiming,
    /// How long to keep an unused session alive.
    keepalive_for: Duration,
    browser: Browser,
    state: Mutex<State>,
    /// Asks the keepalive to check the session now.
    wake: Notify,
}

struct State {
    status: ConnectionStatus,
    /// Set while a login runs; everyone in `connect` waits on it.
    login: Option<Login>,
    /// The agent's last request through this connection.
    last_used: Instant,
}

impl BankIdSession {
    pub fn new(
        context: Context,
        provider: BankIdProvider,
        timing: BankIdTiming,
        keepalive_for: Duration,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(BankIdSession {
            context,
            provider,
            timing,
            keepalive_for,
            browser: Browser::new()?,
            state: Mutex::new(State {
                status: ConnectionStatus::NotConnected,
                login: None,
                last_used: Instant::now(),
            }),
            wake: Notify::new(),
        }))
    }

    /// Restore the session from the vault, check it's still alive, and keep
    /// it so for as long as the process runs.
    pub async fn start(self: &Arc<Self>) {
        self.restore().await;
        tokio::spawn(self.clone().keep_alive());
    }

    pub fn status(&self) -> ConnectionStatus {
        lock(&self.state).status
    }

    /// The session's cookies for `url`, or why there are none.
    pub fn attach(&self, url: &Url) -> Result<Vec<(String, Secret)>, ConnectionStatus> {
        let mut state = lock(&self.state);
        if state.status != ConnectionStatus::Connected {
            return Err(state.status);
        }
        state.last_used = Instant::now();
        let cookies = self.browser.cookie_header(url);
        Ok(cookies
            .map(|cookies| ("Cookie".to_owned(), cookies))
            .into_iter()
            .collect())
    }

    /// Learn from a response to one of the agent's requests.
    pub fn observe(&self, response: &ResponseHead) {
        self.browser.remember(response);
        if self.provider.signed_out(response) {
            let was_connected = self.status() == ConnectionStatus::Connected;
            self.end(ConnectionStatus::Expired);
            if was_connected {
                self.notice_ended();
            }
        } else if !response.set_cookies.is_empty() {
            self.save_cookies();
        }
    }

    /// Log in, or join the login in progress. The login runs on its own, to
    /// the order's lifetime, so a later `connect` can still catch it.
    pub async fn connect(self: &Arc<Self>, timeout: Duration) -> Result<ConnectionStatus, String> {
        let Some(waiting) = self.begin() else {
            return Ok(ConnectionStatus::Connected);
        };
        login::answer(&self.context.name, waiting.wait(timeout).await, timeout)
    }

    /// Start a login, or join the one in progress. `None` if already connected.
    fn begin(self: &Arc<Self>) -> Option<Waiting> {
        let mut state = lock(&self.state);
        if state.status == ConnectionStatus::Connected {
            return None;
        }
        if let Some(login) = &state.login {
            return Some(login.join());
        }
        let (login, waiting) = Login::new();
        let before = state.status;
        state.login = Some(login);
        state.status = ConnectionStatus::LoggingIn;
        drop(state);
        tokio::spawn(self.clone().run_login(before));
        Some(waiting)
    }

    async fn run_login(self: Arc<Self>, before: ConnectionStatus) {
        let name = &self.context.name;
        let result = self.log_in().await;
        let login = {
            let mut state = lock(&self.state);
            state.status = if result.is_ok() {
                ConnectionStatus::Connected
            } else {
                before
            };
            state.last_used = Instant::now();
            state.login.take()
        };
        let result = result.map_err(|reason| format!("Signing in to {name} failed: {reason}"));
        match &result {
            Ok(()) => {
                self.save_cookies();
                self.wake.notify_one();
                self.context.notice(format!("{name} is connected."));
            }
            Err(message) => {
                self.browser.clear();
                self.context.notice(message.clone());
            }
        }
        if let Some(login) = login {
            login.finish(result);
        }
    }

    async fn log_in(&self) -> Result<(), String> {
        self.browser.clear();
        let (mut order, token) = self.provider.start(&self.browser).await?;
        self.context.notice(format!(
            "Sign in to {} with BankID on your phone: {}",
            self.context.name,
            bankid::autostart_link(&token)
        ));
        let deadline = Instant::now() + self.timing.order_lifetime;
        loop {
            tokio::time::sleep(self.timing.poll_every).await;
            match self.provider.poll(&self.browser, &mut order).await? {
                Progress::Complete => break,
                Progress::Failed(reason) => return Err(format!("BankID said {reason}")),
                Progress::Pending if Instant::now() >= deadline => {
                    self.provider.cancel(&self.browser, &order).await;
                    return Err("nobody approved it in BankID in time".into());
                }
                Progress::Pending => {}
            }
        }
        self.provider.finish(&self.browser, order).await
    }

    async fn restore(&self) {
        let saved = match self.context.vault.get(COOKIES) {
            Ok(Some(saved)) => saved,
            Ok(None) => return,
            Err(error) => return self.context.debug(&error.to_string()),
        };
        if let Err(error) = self.browser.restore(&saved) {
            return self.context.debug(&format!("restoring cookies failed: {error}"));
        }
        {
            let mut state = lock(&self.state);
            state.status = ConnectionStatus::Connected;
            state.last_used = Instant::now();
        }
        self.check().await;
    }

    /// Keep the session alive while the agent uses it. A session unused for
    /// `keepalive_for` is let go.
    async fn keep_alive(self: Arc<Self>) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(self.timing.keepalive_every) => {}
                () = self.wake.notified() => continue,
            }
            let (status, idle) = {
                let state = lock(&self.state);
                (state.status, state.last_used.elapsed())
            };
            if status != ConnectionStatus::Connected {
                continue;
            }
            if idle > self.keepalive_for {
                let unused = humantime::format_duration(Duration::from_secs(idle.as_secs() / 60 * 60));
                self.end(ConnectionStatus::NotConnected);
                self.context
                    .notice(format!("Signed out of {} after {unused} unused.", self.context.name));
                continue;
            }
            self.check().await;
        }
    }

    /// One keepalive request: it renews the session, or shows it has ended.
    async fn check(&self) {
        let url = self.provider.keepalive();
        let response = self
            .browser
            .http()
            .get(url.clone())
            .header("accept", "application/json")
            .send()
            .await;
        match response {
            Ok(response) => {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok());
                let head = ResponseHead {
                    location: location.map(str::to_owned),
                    ..ResponseHead::status(url, response.status().as_u16())
                };
                if self.provider.signed_out(&head) {
                    self.end(ConnectionStatus::Expired);
                    self.notice_ended();
                } else {
                    self.save_cookies();
                }
            }
            Err(error) => self.context.debug(&format!("keepalive failed: {error}")),
        }
    }

    fn end(&self, status: ConnectionStatus) {
        lock(&self.state).status = status;
        self.browser.clear();
        let _ = self.context.vault.delete(COOKIES);
    }

    fn notice_ended(&self) {
        self.context.notice(format!(
            "{}'s session has ended; it needs signing in again.",
            self.context.name
        ));
    }

    fn save_cookies(&self) {
        let saved = self
            .browser
            .save()
            .and_then(|cookies| self.context.vault.put(COOKIES, &cookies).map_err(|e| e.to_string()));
        if let Err(error) = saved {
            self.context.debug(&format!("keeping the session failed: {error}"));
        }
    }
}
