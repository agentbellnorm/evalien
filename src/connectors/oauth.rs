//! OAuth 2.1, the steps every provider shares: the authorization URL with
//! PKCE, exchanging the code, and refreshing. A provider is endpoints plus
//! whatever extra parameters it needs.

use std::time::Duration;

use base64::Engine;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::domain::Secret;

/// An OAuth client for one connection.
pub struct OAuthClient {
    authorize: Url,
    token: Url,
    client_id: String,
    client_secret: Secret,
    scopes: Vec<String>,
    /// Provider-specific authorization parameters.
    extra: Vec<(&'static str, &'static str)>,
}

/// What a successful token request returns.
pub struct Tokens {
    pub access: Secret,
    pub expires_in: Duration,
    /// Present after a code exchange; after a refresh only if it rotated.
    pub refresh: Option<Secret>,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The provider refused the grant: it was revoked, expired or never valid.
    /// Only a new login helps.
    #[error("the provider refused: {0}")]
    Refused(String),
    /// Anything else, likely temporary.
    #[error("{0}")]
    Failed(String),
}

/// A PKCE verifier and its challenge (S256).
pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self, getrandom::Error> {
        let verifier = random_token()?;
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        Ok(Pkce {
            verifier: Secret::new(verifier),
            challenge,
        })
    }
}

/// 256 random bits, URL-safe. Used for `state` and PKCE verifiers.
pub fn random_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(base64url(&bytes))
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl OAuthClient {
    /// A provider with these endpoints and no extra parameters.
    pub fn new(authorize: Url, token: Url, client_id: String, client_secret: Secret, scopes: Vec<String>) -> Self {
        OAuthClient {
            authorize,
            token,
            client_id,
            client_secret,
            scopes,
            extra: Vec::new(),
        }
    }

    /// Extra authorization parameters the provider needs.
    pub fn with_parameters(self, extra: Vec<(&'static str, &'static str)>) -> Self {
        OAuthClient { extra, ..self }
    }

    pub fn authorization_url(&self, redirect: &Url, state: &str, challenge: &str) -> Url {
        let mut url = self.authorize.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect.as_str())
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .extend_pairs(&self.extra);
        url
    }

    pub async fn exchange(
        &self,
        http: &reqwest::Client,
        redirect: &Url,
        code: &str,
        verifier: &Secret,
    ) -> Result<Tokens, OAuthError> {
        self.request(
            http,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect.as_str()),
                ("code_verifier", verifier.expose()),
            ],
        )
        .await
    }

    pub async fn refresh(&self, http: &reqwest::Client, refresh: &Secret) -> Result<Tokens, OAuthError> {
        self.request(
            http,
            &[("grant_type", "refresh_token"), ("refresh_token", refresh.expose())],
        )
        .await
    }

    async fn request(&self, http: &reqwest::Client, grant: &[(&str, &str)]) -> Result<Tokens, OAuthError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(grant)
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", self.client_secret.expose())
            .finish();
        let response = http
            .post(self.token.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthError::Failed(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| OAuthError::Failed(e.to_string()))?;
        if !status.is_success() {
            return Err(refusal(status.as_u16(), &text));
        }
        let tokens: TokenResponse =
            serde_json::from_str(&text).map_err(|e| OAuthError::Failed(format!("unexpected token response: {e}")))?;
        Ok(Tokens {
            access: Secret::new(tokens.access_token),
            expires_in: Duration::from_secs(tokens.expires_in.unwrap_or(3600)),
            refresh: tokens.refresh_token.map(Secret::new),
        })
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

/// RFC 6749 error codes that mean the grant itself is no good.
fn refusal(status: u16, body: &str) -> OAuthError {
    match serde_json::from_str::<ErrorResponse>(body) {
        Ok(error) => {
            let message = match error.error_description {
                Some(description) => format!("{}: {description}", error.error),
                None => error.error.clone(),
            };
            match error.error.as_str() {
                "invalid_grant" | "invalid_client" | "unauthorized_client" => OAuthError::Refused(message),
                _ => OAuthError::Failed(message),
            }
        }
        Err(_) => OAuthError::Failed(format!("{status}: {}", body.chars().take(200).collect::<String>())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorization_url_carries_pkce_state_scopes_and_the_providers_parameters() {
        let client = OAuthClient::new(
            Url::parse("https://accounts.example/authorize").unwrap(),
            Url::parse("https://accounts.example/token").unwrap(),
            "client-1".into(),
            Secret::new("shh"),
            vec!["calendar.readonly".into(), "openid".into()],
        )
        .with_parameters(vec![("access_type", "offline")]);
        let redirect = Url::parse("https://evalien.bushnet.diy/oauth/callback").unwrap();
        let url = client.authorization_url(&redirect, "state-123", "challenge-abc");
        let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("accounts.example"));
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["redirect_uri"], "https://evalien.bushnet.diy/oauth/callback");
        assert_eq!(query["scope"], "calendar.readonly openid");
        assert_eq!(
            (query["state"].as_str(), query["code_challenge"].as_str()),
            ("state-123", "challenge-abc")
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["access_type"], "offline");
        assert!(
            !url.as_str().contains("shh"),
            "the client secret never goes to the browser"
        );
    }

    #[test]
    fn pkce_is_s256_of_a_random_verifier() {
        let pkce = Pkce::new().unwrap();
        assert_eq!(pkce.verifier.expose().len(), 43);
        assert_eq!(
            pkce.challenge,
            base64url(&Sha256::digest(pkce.verifier.expose().as_bytes()))
        );
        assert_ne!(Pkce::new().unwrap().verifier.expose(), pkce.verifier.expose());
        // RFC 7636's example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&Sha256::digest(verifier)),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_refused_grant_means_logging_in_again_and_anything_else_is_temporary() {
        assert!(
            matches!(refusal(400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#), OAuthError::Refused(m) if m.contains("revoked"))
        );
        assert!(matches!(
            refusal(400, r#"{"error":"invalid_request"}"#),
            OAuthError::Failed(_)
        ));
        assert!(matches!(refusal(503, "<html>down</html>"), OAuthError::Failed(m) if m.starts_with("503")));
    }
}
