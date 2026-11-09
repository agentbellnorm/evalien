//! Mobile BankID logins, the steps every service shares: start an order,
//! hand the human the autostart link, poll until it's approved, then let the
//! service turn the approved order into a logged-in session (cookies).
//!
//! Each service implements those steps; the session does everything around
//! them (the link, the poll loop and its deadline, keeping the session alive).

use std::time::Duration;

use reqwest::Url;

use crate::domain::{ResponseHead, Secret};

use super::{
    browser::Browser,
    seb::{Seb, SebLogin},
};

/// Where the autostart link points: the BankID app on the human's phone.
const APP: &str = "https://app.bankid.com/";

/// How a BankID connection paces itself.
#[derive(Clone, Copy, Debug)]
pub struct BankIdTiming {
    /// How often to ask whether the human has approved.
    pub poll_every: Duration,
    /// How long to wait for the human before withdrawing the order. BankID
    /// expires orders after three minutes anyway.
    pub order_lifetime: Duration,
    /// How often to check that a session is alive, and keep it so.
    pub keepalive_every: Duration,
}

impl Default for BankIdTiming {
    fn default() -> Self {
        BankIdTiming {
            poll_every: Duration::from_secs(2),
            order_lifetime: Duration::from_secs(180),
            keepalive_every: Duration::from_secs(4 * 60),
        }
    }
}

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
    format!("{APP}?autostarttoken={}&redirect=null", token.expose())
}

/// The autostart link in a notice, for showing it some other way, like a QR code.
pub fn find_link(notice: &str) -> Option<&str> {
    notice.split_whitespace().find(|word| word.starts_with(APP))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_in_a_notice_can_be_found_again() {
        let link = autostart_link(&Secret::new("ast-1"));
        assert_eq!(
            find_link(&format!("Sign in to seb with BankID on your phone: {link}")),
            Some(link.as_str())
        );
        assert_eq!(find_link("seb is connected."), None);
    }
}
