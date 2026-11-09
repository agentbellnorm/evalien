//! An HTTP client that keeps cookies, for services whose sessions are
//! cookies: the login steps use it like a browser would, and the agent's
//! requests to the same service read and update the same jar.

use std::{
    sync::{Arc, PoisonError},
    time::Duration,
};

use cookie_store::CookieStore;
use reqwest::Url;
use reqwest_cookie_store::CookieStoreMutex;

use crate::domain::{ResponseHead, Secret};

/// Web logins expect a browser, and some reject anything else.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:145.0) Gecko/20100101 Firefox/145.0";

pub struct Browser {
    http: reqwest::Client,
    jar: Arc<CookieStoreMutex>,
}

impl Browser {
    pub fn new() -> Result<Self, String> {
        let jar = Arc::new(CookieStoreMutex::new(CookieStore::new()));
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_provider(jar.clone())
            // Login steps follow redirects themselves, to read where they lead.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Browser { http, jar })
    }

    /// For login steps. Cookies go out and come back by themselves.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The `Cookie` header for a request to `url`, if any cookie applies.
    pub fn cookie_header(&self, url: &Url) -> Option<Secret> {
        let jar = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        let pairs: Vec<String> = jar
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        (!pairs.is_empty()).then(|| Secret::new(pairs.join("; ")))
    }

    /// Keep the cookies a response to one of the agent's requests set.
    pub fn remember(&self, response: &ResponseHead) {
        let mut jar = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        for cookie in &response.set_cookies {
            // A cookie the jar refuses (wrong domain, malformed) is just not kept.
            let _ = jar.parse(cookie, &response.url);
        }
    }

    pub fn has_cookie(&self, url: &Url, name: &str) -> bool {
        let jar = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        jar.get_request_values(url).any(|(cookie, _)| cookie == name)
    }

    pub fn clear(&self) {
        self.jar.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// Every cookie, session cookies included, as JSON for the vault.
    pub fn save(&self) -> Result<Secret, String> {
        let jar = self.jar.lock().unwrap_or_else(PoisonError::into_inner);
        let mut json = Vec::new();
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&jar, &mut json).map_err(|e| e.to_string())?;
        String::from_utf8(json).map(Secret::new).map_err(|e| e.to_string())
    }

    pub fn restore(&self, saved: &Secret) -> Result<(), String> {
        let store = cookie_store::serde::json::load_all(saved.expose().as_bytes()).map_err(|e| e.to_string())?;
        *self.jar.lock().unwrap_or_else(PoisonError::into_inner) = store;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn cookies_follow_their_domain_and_survive_a_save() {
        let browser = Browser::new().unwrap();
        let login = url("https://id.seb.se/th/login/v1/end");
        browser.remember(&ResponseHead {
            url: login.clone(),
            status: 302,
            set_cookies: vec![
                "seb-session-id=abc; Domain=seb.se; Path=/; Secure; HttpOnly".into(),
                "only-id=1; Path=/; Secure".into(),
            ],
            location: None,
        });
        let apps = url("https://apps.seb.se/ssc/accounts-widget/api/accounts/all");
        assert_eq!(browser.cookie_header(&apps).unwrap().expose(), "seb-session-id=abc");
        assert!(browser.cookie_header(&url("https://example.com/")).is_none());
        assert!(
            browser.cookie_header(&url("http://apps.seb.se/")).is_none(),
            "Secure cookies need https"
        );

        let saved = browser.save().unwrap();
        let restored = Browser::new().unwrap();
        restored.restore(&saved).unwrap();
        assert!(restored.has_cookie(&apps, "seb-session-id"));
        restored.clear();
        assert!(restored.cookie_header(&apps).is_none());
    }
}
