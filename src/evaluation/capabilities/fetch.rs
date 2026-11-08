//! HTTP for the agent, limited to the public internet. Addresses are checked
//! when connecting, so a hostname that resolves to a private address (DNS
//! rebinding included) can't reach the LAN, loopback or cloud metadata.
//!
//! The credential gateway attaches credentials by origin, hop by hop:
//! redirects are followed here rather than by the HTTP client, so a redirect
//! to another origin never carries the first origin's credentials.

use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use reqwest::{
    Method, StatusCode, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{HeaderMap, HeaderName, HeaderValue, LOCATION},
    redirect,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{Attachment, ConnectionStatus, Gateway, Origin, RequestOutcome};

const MAX_REDIRECTS: usize = 5;
const MAX_BODY: usize = 5 << 20;
/// Tells the agent a connection needs attention, e.g. `kivra; status=expired`.
pub const CONNECTION_HEADER: &str = "x-evalien-connection";
/// Response headers the agent never sees: sessions belong to the gateway.
const HIDDEN_HEADERS: [&str; 3] = ["set-cookie", "set-cookie2", "authorization"];

#[derive(Clone)]
pub struct Fetch {
    http: reqwest::Client,
    gateway: Option<Arc<dyn Gateway>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    url: String,
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

/// One request on the way to a final response.
#[derive(Clone, Debug, PartialEq)]
struct Hop {
    url: Url,
    method: Method,
    body: Option<String>,
}

impl Fetch {
    /// `gateway` attaches credentials for connected services, if there are any.
    pub fn new(gateway: Option<Arc<dyn Gateway>>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent("evalien")
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnly))
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Fetch { http, gateway })
    }

    pub async fn request(&self, request: FetchRequest) -> Result<Value, String> {
        let url = Url::parse(&request.url).map_err(|e| format!("invalid URL: {e}"))?;
        let method = Method::from_bytes(request.method.to_uppercase().as_bytes()).map_err(|e| e.to_string())?;
        let mut hop = Hop {
            url,
            method,
            body: request.body,
        };
        for _ in 0..=MAX_REDIRECTS {
            check_url(&hop.url)?;
            let attachment = self.attach(&hop.url);
            let mut builder = self
                .http
                .request(hop.method.clone(), hop.url.clone())
                .headers(headers(&request.headers, &attachment)?);
            if let Some(body) = &hop.body {
                builder = builder.body(body.clone());
            }
            let sent = builder.send().await;
            let hint = self.observe(&attachment, sent.as_ref().ok().map(|response| response.status()));
            let response = sent.map_err(|e| describe(&e))?;
            let location = response.headers().get(LOCATION).and_then(|value| value.to_str().ok());
            match next_hop(&hop, response.status(), location) {
                Some(next) => hop = next,
                None => return read(response, hint).await,
            }
        }
        Err(format!("more than {MAX_REDIRECTS} redirects"))
    }

    fn attach(&self, url: &Url) -> Attachment {
        match &self.gateway {
            Some(gateway) => gateway.attach(&Origin::of(url)),
            None => Attachment::Nothing,
        }
    }

    /// Report how a request through a connection went. Returns the hint for
    /// the agent when the connection now needs attention.
    fn observe(&self, attachment: &Attachment, status: Option<StatusCode>) -> Option<String> {
        let (Some(gateway), Attachment::Credentials { connection, .. }) = (&self.gateway, attachment) else {
            return None;
        };
        let outcome = status.map_or(RequestOutcome::Failed, |status| RequestOutcome::Status(status.as_u16()));
        match gateway.observe(connection, outcome) {
            ConnectionStatus::Connected => None,
            status => Some(format!("{connection}; status={}", status.as_str())),
        }
    }
}

/// The agent's headers, with the gateway's credentials replacing any of the same name.
fn headers(agent: &BTreeMap<String, String>, attachment: &Attachment) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (name, value) in agent {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| format!("header {name:?}: {e}"))?;
        let value = HeaderValue::from_str(value).map_err(|e| format!("header {name}: {e}"))?;
        headers.append(name, value);
    }
    if let Attachment::Credentials {
        headers: credentials, ..
    } = attachment
    {
        for (name, secret) in credentials {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|e| format!("connection header {name:?}: {e}"))?;
            let mut value =
                HeaderValue::from_str(secret.expose()).map_err(|_| format!("connection header {name} is invalid"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }
    Ok(headers)
}

/// Where a redirect leads, as browsers follow them: 303, and 301/302 after a
/// POST, continue as a GET without the body; 307 and 308 repeat the request.
fn next_hop(hop: &Hop, status: StatusCode, location: Option<&str>) -> Option<Hop> {
    if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let url = hop.url.join(location?).ok()?;
    let becomes_get = status == StatusCode::SEE_OTHER && hop.method != Method::HEAD
        || matches!(status.as_u16(), 301 | 302) && hop.method == Method::POST;
    if becomes_get {
        return Some(Hop {
            url,
            method: Method::GET,
            body: None,
        });
    }
    Some(Hop { url, ..hop.clone() })
}

/// The response as the agent sees it: no session headers, and a hint when a
/// connection needs attention.
async fn read(mut response: reqwest::Response, hint: Option<String>) -> Result<Value, String> {
    let status = response.status().as_u16();
    let url = response.url().to_string();
    let mut headers: BTreeMap<String, String> = response
        .headers()
        .iter()
        .filter(|(name, _)| !HIDDEN_HEADERS.contains(&name.as_str()))
        .map(|(name, value)| (name.to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
        .collect();
    if let Some(hint) = hint {
        headers.insert(CONNECTION_HEADER.to_owned(), hint);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(format!("response body is larger than {MAX_BODY} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(json!({ "status": status, "url": url, "headers": headers, "body": String::from_utf8_lossy(&body) }))
}

/// Only http and https, and never an address literal outside the public internet.
fn check_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("only http and https are allowed, not {}", url.scheme()));
    }
    let address = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        Some(url::Host::Domain(_)) => None,
        None => return Err("the URL has no host".into()),
    };
    match address {
        Some(ip) if !is_public(ip) => Err(format!("{ip} is not a public address")),
        _ => Ok(()),
    }
}

/// Resolves names, keeping only public addresses.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|address| is_public(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(format!("{host} does not resolve to a public address").into());
            }
            let addresses: Addrs = Box::new(addresses.into_iter());
            Ok(addresses)
        })
    }
}

/// Whether an address is on the public internet: not loopback, private,
/// link-local (cloud metadata lives there), shared, reserved or documentation.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || s[..6].iter().all(|segment| *segment == 0)
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x0064 && s[1] == 0xff9b)
                || s[0] == 0x2002)
        }
    }
}

/// reqwest wraps the reason a request was refused; show the innermost cause.
fn describe(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Secret;

    fn hop(method: Method, url: &str) -> Hop {
        Hop {
            url: Url::parse(url).unwrap(),
            method,
            body: Some("payload".into()),
        }
    }

    #[test]
    fn credentials_replace_the_agents_headers_of_the_same_name() {
        let agent: BTreeMap<String, String> = [
            ("authorization".to_owned(), "Bearer forged".to_owned()),
            ("accept".to_owned(), "application/json".to_owned()),
        ]
        .into_iter()
        .collect();
        let attachment = Attachment::Credentials {
            connection: "example".into(),
            headers: vec![("Authorization".into(), Secret::new("Bearer real"))],
        };
        let headers = headers(&agent, &attachment).unwrap();
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(headers["authorization"], "Bearer real");
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(headers["accept"], "application/json");
        assert!(super::headers(&agent, &Attachment::Nothing).unwrap()["authorization"] == "Bearer forged");
    }

    #[test]
    fn redirects_are_followed_the_way_browsers_follow_them() {
        let post = hop(Method::POST, "https://a.example/form");
        let found = next_hop(&post, StatusCode::FOUND, Some("/done")).unwrap();
        assert_eq!(
            (found.url.as_str(), found.method, found.body),
            ("https://a.example/done", Method::GET, None)
        );
        let kept = next_hop(&post, StatusCode::TEMPORARY_REDIRECT, Some("https://b.example/x")).unwrap();
        assert_eq!(
            (kept.url.as_str(), kept.method, kept.body.as_deref()),
            ("https://b.example/x", Method::POST, Some("payload"))
        );
        let see_other = next_hop(
            &hop(Method::PUT, "https://a.example/"),
            StatusCode::SEE_OTHER,
            Some("/r"),
        )
        .unwrap();
        assert_eq!(see_other.method, Method::GET);
        assert!(next_hop(&post, StatusCode::OK, Some("/x")).is_none());
        assert!(next_hop(&post, StatusCode::FOUND, None).is_none());
    }
}
