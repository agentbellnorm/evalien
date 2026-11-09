//! Mobile BankID logins, the steps every service shares: start an order,
//! hand the human the autostart link, poll until it's approved, then let the
//! service turn the approved order into a logged-in session (cookies).
//!
//! Each service implements those steps; the gateway does everything around
//! them (the link, the poll loop and its deadline, keeping the session alive).

use std::time::Duration;

use reqwest::Url;

use crate::domain::{ResponseHead, Secret};

use super::{
    browser::Browser,
    seb::{Seb, SebLogin},
};

/// A BankID order expires after a few minutes if nobody approves it.
pub const ORDER_LIFETIME: Duration = Duration::from_secs(180);

/// The services with a BankID login.
pub enum BankIdProvider {
    Seb(Seb),
}

/// A login in progress, as the service needs it to continue.
pub enum Login {
    Seb(SebLogin),
}

/// What polling an order says.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    /// Not approved yet.
    Pending,
    /// Approved: ready to finish.
    Complete,
    /// Declined, expired, or failed, with BankID's reason.
    Failed(String),
}

impl BankIdProvider {
    /// A known provider, by the name used in config.
    pub fn for_name(name: &str) -> Option<Self> {
        match name {
            "seb" => Some(BankIdProvider::Seb(Seb::production())),
            _ => None,
        }
    }

    /// Start an order. Returns the login and the autostart token for the human.
    pub async fn start(&self, browser: &Browser) -> Result<(Login, Secret), String> {
        match self {
            BankIdProvider::Seb(seb) => seb
                .start(browser)
                .await
                .map(|(login, token)| (Login::Seb(login), token)),
        }
    }

    pub async fn poll(&self, browser: &Browser, login: &mut Login) -> Result<Progress, String> {
        match (self, login) {
            (BankIdProvider::Seb(seb), Login::Seb(login)) => seb.poll(browser, login).await,
        }
    }

    /// Turn an approved order into a session.
    pub async fn finish(&self, browser: &Browser, login: Login) -> Result<(), String> {
        match (self, login) {
            (BankIdProvider::Seb(seb), Login::Seb(login)) => seb.finish(browser, login).await,
        }
    }

    /// Withdraw an order nobody approved.
    pub async fn cancel(&self, browser: &Browser, login: &Login) {
        match (self, login) {
            (BankIdProvider::Seb(seb), Login::Seb(login)) => seb.cancel(browser, login).await,
        }
    }

    pub fn poll_every(&self) -> Duration {
        match self {
            BankIdProvider::Seb(seb) => seb.poll_every(),
        }
    }

    /// How long to wait for the human to approve before withdrawing the order.
    pub fn order_lifetime(&self) -> Duration {
        match self {
            BankIdProvider::Seb(seb) => seb.order_lifetime(),
        }
    }

    /// A cheap request that shows the session is alive, and keeps it so.
    pub fn keepalive(&self) -> Url {
        match self {
            BankIdProvider::Seb(seb) => seb.keepalive(),
        }
    }

    /// Whether a response says the session has ended.
    pub fn signed_out(&self, response: &ResponseHead) -> bool {
        match self {
            BankIdProvider::Seb(seb) => seb.signed_out(response),
        }
    }
}

/// The link that opens the BankID app on the human's phone. `redirect=null`
/// leaves them in the app once they've approved.
pub fn autostart_link(token: &Secret) -> String {
    format!(
        "https://app.bankid.com/?autostarttoken={}&redirect=null",
        token.expose()
    )
}
