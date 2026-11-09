//! The services evalien knows, by the names config uses: which
//! implementation each name means, and the skill that comes with it.

use std::time::Duration;

use reqwest::Url;

use crate::domain::{Origin, Secret};

use super::{
    api_key::ApiKey,
    bankid::{BankIdProvider, BankIdTiming},
    config::{ConnectionConfig, KindConfig},
    oauth::OAuthClient,
    seb::Seb,
};

/// A connection with its implementation chosen, ready to start.
pub struct Service {
    pub name: String,
    pub origins: Vec<Origin>,
    /// How the agent uses the service, if evalien knows.
    pub skill: Option<&'static str>,
    pub credentials: Credentials,
}

pub enum Credentials {
    ApiKey(ApiKey),
    OAuth {
        client: OAuthClient,
        /// Where the provider sends the browser back to: the callback.
        redirect: Url,
    },
    BankId {
        provider: BankIdProvider,
        timing: BankIdTiming,
        /// How long to keep an unused session alive.
        keepalive_for: Duration,
    },
}

/// Each configured connection as a service. `redirect` is the OAuth callback's URL.
pub fn resolve(connections: Vec<ConnectionConfig>, redirect: Option<&Url>) -> Result<Vec<Service>, String> {
    connections
        .into_iter()
        .map(|connection| {
            let name = connection.name;
            let fail = |message: String| format!("connection {name}: {message}");
            let (credentials, skill) = match connection.kind {
                KindConfig::ApiKey { header, value } => (Credentials::ApiKey(ApiKey::new(header, value)), None),
                KindConfig::OAuth {
                    provider,
                    client_id,
                    client_secret,
                    scopes,
                } => {
                    let client = oauth(&provider, client_id, client_secret, scopes)
                        .ok_or_else(|| fail(format!("unknown OAuth provider {provider:?}; known: google")))?;
                    let redirect = redirect
                        .cloned()
                        .ok_or_else(|| fail("OAuth needs a callback: set OAUTH_REDIRECT_URL".into()))?;
                    (Credentials::OAuth { client, redirect }, None)
                }
                KindConfig::BankId {
                    provider,
                    keepalive_for,
                } => {
                    let (provider, skill) = bankid(&provider)
                        .ok_or_else(|| fail(format!("unknown BankID provider {provider:?}; known: seb")))?;
                    let credentials = Credentials::BankId {
                        provider,
                        timing: BankIdTiming::default(),
                        keepalive_for,
                    };
                    (credentials, Some(skill))
                }
            };
            Ok(Service {
                name,
                origins: connection.origins,
                skill,
                credentials,
            })
        })
        .collect()
}

fn oauth(provider: &str, client_id: String, client_secret: Secret, scopes: Vec<String>) -> Option<OAuthClient> {
    match provider {
        "google" => Some(
            OAuthClient::new(
                Url::parse("https://accounts.google.com/o/oauth2/v2/auth").ok()?,
                Url::parse("https://oauth2.googleapis.com/token").ok()?,
                client_id,
                client_secret,
                scopes,
            )
            // A refresh token, every time the human consents.
            .with_parameters(vec![
                ("access_type", "offline"),
                ("prompt", "consent"),
                ("include_granted_scopes", "true"),
            ]),
        ),
        _ => None,
    }
}

fn bankid(provider: &str) -> Option<(BankIdProvider, &'static str)> {
    match provider {
        "seb" => Some((BankIdProvider::Seb(Seb::production()), Seb::SKILL)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(kind: KindConfig) -> Vec<ConnectionConfig> {
        vec![ConnectionConfig {
            name: "c".into(),
            origins: vec![Origin::parse("https://example.com").unwrap()],
            kind,
        }]
    }

    fn oauth_config(provider: &str) -> Vec<ConnectionConfig> {
        connection(KindConfig::OAuth {
            provider: provider.into(),
            client_id: "id-1".into(),
            client_secret: Secret::new("shh"),
            scopes: vec!["calendar".into()],
        })
    }

    #[test]
    fn google_asks_for_offline_access_at_the_callback() {
        let redirect = Url::parse("https://evalien.bushnet.diy/oauth/callback").unwrap();
        let services = resolve(oauth_config("google"), Some(&redirect)).unwrap();
        let Credentials::OAuth { client, redirect } = &services[0].credentials else {
            panic!("expected OAuth")
        };
        let url = client.authorization_url(redirect, "state", "challenge");
        let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(
            (query["access_type"].as_str(), query["prompt"].as_str()),
            ("offline", "consent")
        );
        assert_eq!(query["redirect_uri"], "https://evalien.bushnet.diy/oauth/callback");
    }

    #[test]
    fn seb_comes_with_its_skill() {
        let services = resolve(
            connection(KindConfig::BankId {
                provider: "seb".into(),
                keepalive_for: Duration::from_secs(60),
            }),
            None,
        )
        .unwrap();
        assert!(services[0].skill.is_some_and(|skill| skill.contains("accounts")));
    }

    #[test]
    fn unknown_providers_and_a_missing_callback_are_refused() {
        let redirect = Url::parse("https://evalien.test/oauth/callback").unwrap();
        let error = |result: Result<Vec<Service>, String>| result.err().unwrap();
        assert!(error(resolve(oauth_config("myspace"), Some(&redirect))).contains("unknown OAuth provider"));
        assert!(error(resolve(oauth_config("google"), None)).contains("OAUTH_REDIRECT_URL"));
        let nordea = connection(KindConfig::BankId {
            provider: "nordea".into(),
            keepalive_for: Duration::from_secs(60),
        });
        assert!(error(resolve(nordea, None)).contains("unknown BankID provider"));
    }
}
