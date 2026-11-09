//! The credential gateway, as the rest of the runtime sees it. Connections
//! attach credentials to the agent's requests by origin, so the agent calls
//! services directly and never holds a secret.

use std::{fmt, future::Future, pin::Pin, time::Duration};

use super::event::Secret;

/// Where a request goes: scheme, host, and port if it isn't the default.
/// Displays as `https://api.example.com`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Origin(String);

impl Origin {
    pub fn of(url: &url::Url) -> Self {
        Origin(url.origin().ascii_serialization())
    }

    /// An origin written in config: HTTPS, with nothing after the host and port.
    pub fn parse(text: &str) -> Result<Self, String> {
        let url = url::Url::parse(text).map_err(|e| format!("{text}: {e}"))?;
        if url.scheme() != "https" {
            return Err(format!("{text}: credentials only go over https"));
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
            return Err(format!("{text}: an origin is just https://host[:port]"));
        }
        Ok(Origin::of(&url))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    ApiKey,
    OAuth,
    BankId,
}

impl ConnectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionKind::ApiKey => "api_key",
            ConnectionKind::OAuth => "oauth",
            ConnectionKind::BankId => "bankid",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Never signed in, or signed out.
    NotConnected,
    /// Waiting for the human to finish signing in.
    LoggingIn,
    /// Requests to its origins carry credentials.
    Connected,
    /// The service revoked the credentials, or the session ended.
    Expired,
}

impl ConnectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionStatus::NotConnected => "not_connected",
            ConnectionStatus::LoggingIn => "logging_in",
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Expired => "expired",
        }
    }
}

/// How requests through one connection have gone since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCounts {
    pub succeeded: u64,
    pub rejected: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub failed: u64,
}

/// A connection and its state, for the agent and for metrics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub name: String,
    pub kind: ConnectionKind,
    pub status: ConnectionStatus,
    pub origins: Vec<Origin>,
    pub requests: RequestCounts,
    /// Whether there's a skill describing how to use the service.
    pub skill: bool,
}

/// What the gateway adds to one request.
#[derive(Debug, PartialEq, Eq)]
pub enum Attachment {
    /// No connection serves this origin: the request goes out as written.
    Nothing,
    /// Headers that replace any of the same name the agent set.
    Credentials {
        connection: String,
        headers: Vec<(String, Secret)>,
    },
    /// A connection serves this origin but isn't signed in: the request goes
    /// out as written, and the agent is told.
    NotConnected {
        connection: String,
        status: ConnectionStatus,
    },
}

/// How a request through a connection ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestOutcome {
    Response(ResponseHead),
    /// No response: DNS, connection or timeout.
    Failed,
}

/// What a connection learns from a response: sessions renew their cookies,
/// and a signed-out session often shows itself as a redirect to a login page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub url: url::Url,
    pub status: u16,
    pub set_cookies: Vec<String>,
    pub location: Option<String>,
}

impl ResponseHead {
    /// A response with just a status, as for an API that sets no cookies.
    pub fn status(url: url::Url, status: u16) -> Self {
        ResponseHead {
            url,
            status,
            set_cookies: Vec::new(),
            location: None,
        }
    }
}

/// A login in progress. Resolves to the connection's status once it's done.
pub type Connecting<'a> = Pin<Box<dyn Future<Output = Result<ConnectionStatus, String>> + Send + 'a>>;

pub trait Gateway: Send + Sync {
    /// What to add to a request for `url`.
    fn attach(&self, url: &url::Url) -> Attachment;
    /// Learn from how a request through `connection` went, and return the
    /// connection's status afterwards.
    fn observe(&self, connection: &str, outcome: RequestOutcome) -> ConnectionStatus;
    fn connections(&self) -> Vec<ConnectionInfo>;
    /// How to use a connection's service: its endpoints and what they're for.
    fn skill(&self, connection: &str) -> Option<String>;
    /// Sign in to `connection` if it isn't already, waiting at most `timeout`
    /// for the human. Concurrent calls wait on the same login.
    fn connect(&self, connection: &str, timeout: Duration) -> Connecting<'_>;
}
