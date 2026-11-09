//! A BankID connection's session: logging in, keeping the session's cookies,
//! keeping it alive while the agent uses it, and noticing when it ends.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::{Notify, watch};

use crate::domain::{ConnectionStatus, ResponseHead};

use super::{
    Connection, Connectors, Credentials, Waiting,
    bankid::{self, BankIdProvider, Progress},
    browser::Browser,
    lock,
};

/// How often to check that a session is alive, and keep it so.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(4 * 60);
/// The vault item holding a BankID connection's cookies.
const COOKIES: &str = "cookies";

pub(super) struct BankId {
    provider: BankIdProvider,
    pub(super) browser: Browser,
    keepalive_for: Duration,
    pub(super) session: Mutex<BankIdSession>,
    /// Asks the keepalive to check the session now.
    wake: Notify,
}

pub(super) struct BankIdSession {
    pub(super) status: ConnectionStatus,
    /// Set while a login runs; everyone in `connect` waits on it.
    login: Option<watch::Sender<Option<Result<(), String>>>>,
    /// The agent's last request through this connection.
    pub(super) last_used: Instant,
}

impl BankId {
    pub(super) fn new(provider: BankIdProvider, keepalive_for: Duration) -> Result<Self, String> {
        Ok(BankId {
            provider,
            browser: Browser::new()?,
            keepalive_for,
            session: Mutex::new(BankIdSession {
                status: ConnectionStatus::NotConnected,
                login: None,
                last_used: Instant::now(),
            }),
            wake: Notify::new(),
        })
    }

    pub(super) fn signed_out(&self, response: &ResponseHead) -> bool {
        self.provider.signed_out(response)
    }
}

impl Connectors {
    /// Start a login, or join the one in progress. `None` if already connected.
    pub(super) fn begin_bankid(&self, index: usize) -> Result<Option<Waiting>, String> {
        let Credentials::BankId(bankid) = &self.connections[index].credentials else {
            return Err("not a BankID connection".into());
        };
        let mut session = lock(&bankid.session);
        if session.status == ConnectionStatus::Connected {
            return Ok(None);
        }
        if let Some(done) = &session.login {
            return Ok(Some(done.subscribe()));
        }
        let this = self.this.upgrade().ok_or("the gateway is shutting down")?;
        let (done, waiting) = watch::channel(None);
        let before = session.status;
        session.login = Some(done);
        session.status = ConnectionStatus::LoggingIn;
        drop(session);
        tokio::spawn(this.run_bankid_login(index, before));
        Ok(Some(waiting))
    }

    /// The login, start to finish. Runs on its own, so it completes even if
    /// the agent stops waiting for it.
    async fn run_bankid_login(self: Arc<Self>, index: usize, before: ConnectionStatus) {
        let connection = &self.connections[index];
        let Credentials::BankId(bankid) = &connection.credentials else {
            return;
        };
        let result = self.bankid_login(connection, bankid).await;
        let done = {
            let mut session = lock(&bankid.session);
            session.status = if result.is_ok() {
                ConnectionStatus::Connected
            } else {
                before
            };
            session.last_used = Instant::now();
            session.login.take()
        };
        match &result {
            Ok(()) => {
                self.save_cookies(connection, bankid);
                bankid.wake.notify_one();
                self.notice(format!("{} is connected.", connection.name));
            }
            Err(reason) => {
                bankid.browser.clear();
                self.notice(format!("Signing in to {} failed: {reason}", connection.name));
            }
        }
        if let Some(done) = done {
            done.send_replace(Some(
                result.map_err(|reason| format!("Signing in to {} failed: {reason}", connection.name)),
            ));
        }
    }

    async fn bankid_login(&self, connection: &Connection, bankid: &BankId) -> Result<(), String> {
        bankid.browser.clear();
        let (mut login, token) = bankid.provider.start(&bankid.browser).await?;
        let link = bankid::autostart_link(&token);
        self.notice(format!(
            "Sign in to {} with BankID on your phone: {link}",
            connection.name
        ));
        let deadline = Instant::now() + bankid.provider.order_lifetime();
        loop {
            tokio::time::sleep(bankid.provider.poll_every()).await;
            match bankid.provider.poll(&bankid.browser, &mut login).await? {
                Progress::Complete => break,
                Progress::Failed(reason) => return Err(format!("BankID said {reason}")),
                Progress::Pending if Instant::now() >= deadline => {
                    bankid.provider.cancel(&bankid.browser, &login).await;
                    return Err("nobody approved it in BankID in time".into());
                }
                Progress::Pending => {}
            }
        }
        bankid.provider.finish(&bankid.browser, login).await
    }

    /// Restore a session from the vault, and check it's still alive.
    pub(super) async fn restore_bankid(&self, connection: &Connection, bankid: &BankId) {
        let Ok(vault) = self.vault() else { return };
        let saved = match vault.get(&connection.name, COOKIES) {
            Ok(Some(saved)) => saved,
            Ok(None) => return,
            Err(error) => return (self.debug)(&format!("connectors: {}: {error}", connection.name)),
        };
        if let Err(error) = bankid.browser.restore(&saved) {
            return (self.debug)(&format!(
                "connectors: {}: restoring cookies failed: {error}",
                connection.name
            ));
        }
        {
            let mut session = lock(&bankid.session);
            session.status = ConnectionStatus::Connected;
            session.last_used = Instant::now();
        }
        self.check_bankid(connection, bankid).await;
    }

    /// Keep one connection's session alive while the agent uses it, for as
    /// long as the process runs. A session unused for `keepalive_for` is let go.
    pub(super) async fn keep_bankid_alive(self: Arc<Self>, index: usize) {
        let connection = &self.connections[index];
        let Credentials::BankId(bankid) = &connection.credentials else {
            return;
        };
        loop {
            tokio::select! {
                () = tokio::time::sleep(KEEPALIVE_EVERY) => {}
                () = bankid.wake.notified() => continue,
            }
            let (status, idle) = {
                let session = lock(&bankid.session);
                (session.status, session.last_used.elapsed())
            };
            if status != ConnectionStatus::Connected {
                continue;
            }
            if idle > bankid.keepalive_for {
                let unused = humantime::format_duration(Duration::from_secs(idle.as_secs() / 60 * 60));
                self.end_bankid(connection, bankid, ConnectionStatus::NotConnected);
                self.notice(format!("Signed out of {} after {unused} unused.", connection.name));
                continue;
            }
            self.check_bankid(connection, bankid).await;
        }
    }

    /// One keepalive request: it renews the session, or shows it has ended.
    async fn check_bankid(&self, connection: &Connection, bankid: &BankId) {
        let url = bankid.provider.keepalive();
        match bankid
            .browser
            .http()
            .get(url.clone())
            .header("accept", "application/json")
            .send()
            .await
        {
            Ok(response) => {
                let head = ResponseHead::status(url, response.status().as_u16());
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok());
                let head = ResponseHead {
                    location: location.map(str::to_owned),
                    ..head
                };
                if bankid.signed_out(&head) {
                    self.end_bankid(connection, bankid, ConnectionStatus::Expired);
                    self.notice(format!(
                        "{}'s session has ended; it needs signing in again.",
                        connection.name
                    ));
                } else {
                    self.save_cookies(connection, bankid);
                }
            }
            Err(error) => (self.debug)(&format!("connectors: {}: keepalive failed: {error}", connection.name)),
        }
    }

    /// Learn from a response to one of the agent's requests.
    pub(super) fn observe_bankid(&self, connection: &Connection, bankid: &BankId, response: &ResponseHead) {
        bankid.browser.remember(response);
        if bankid.signed_out(response) {
            let was_connected = lock(&bankid.session).status == ConnectionStatus::Connected;
            self.end_bankid(connection, bankid, ConnectionStatus::Expired);
            if was_connected {
                self.notice(format!(
                    "{}'s session has ended; it needs signing in again.",
                    connection.name
                ));
            }
        } else if !response.set_cookies.is_empty() {
            self.save_cookies(connection, bankid);
        }
    }

    fn end_bankid(&self, connection: &Connection, bankid: &BankId, status: ConnectionStatus) {
        lock(&bankid.session).status = status;
        bankid.browser.clear();
        if let Ok(vault) = self.vault() {
            let _ = vault.delete(&connection.name, COOKIES);
        }
    }

    fn save_cookies(&self, connection: &Connection, bankid: &BankId) {
        let saved = bankid.browser.save().and_then(|cookies| {
            self.vault()?
                .put(&connection.name, COOKIES, &cookies)
                .map_err(|e| e.to_string())
        });
        if let Err(error) = saved {
            (self.debug)(&format!(
                "connectors: {}: keeping the session failed: {error}",
                connection.name
            ));
        }
    }
}
