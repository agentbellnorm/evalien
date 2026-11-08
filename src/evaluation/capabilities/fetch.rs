//! HTTP for the agent, limited to the public internet. Addresses are checked
//! when connecting, so a hostname that resolves to a private address (DNS
//! rebinding included) can't reach the LAN, loopback or cloud metadata.

use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use reqwest::{
    Method, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect,
};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REDIRECTS: usize = 5;
const MAX_BODY: usize = 5 << 20;

#[derive(Clone)]
pub struct Fetch {
    http: reqwest::Client,
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

impl Fetch {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent("evalien")
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnly))
            .redirect(redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if let Err(reason) = check_url(attempt.url()) {
                    attempt.error(reason)
                } else {
                    attempt.follow()
                }
            }))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Fetch { http })
    }

    pub async fn request(&self, request: FetchRequest) -> Result<Value, String> {
        let url = Url::parse(&request.url).map_err(|e| format!("invalid URL: {e}"))?;
        check_url(&url)?;
        let method = Method::from_bytes(request.method.to_uppercase().as_bytes()).map_err(|e| e.to_string())?;
        let mut builder = self.http.request(method, url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }

        let mut response = builder.send().await.map_err(|e| describe(&e))?;
        let status = response.status().as_u16();
        let url = response.url().to_string();
        let headers: BTreeMap<String, String> = response
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(format!("response body is larger than {MAX_BODY} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(json!({ "status": status, "url": url, "headers": headers, "body": String::from_utf8_lossy(&body) }))
    }
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
