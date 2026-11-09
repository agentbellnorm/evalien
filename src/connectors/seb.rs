//! SEB's web login with Mobile BankID, as apps.seb.se does it:
//!
//! 1. `GET id.seb.se/th/login/v1/start` redirects to the login page, whose URL
//!    carries the token handler's `state` and PKCE challenge.
//! 2. The login page sets cookies, and `POST /login/api/login-tokens/v1` the login token.
//! 3. `POST /auth/bid/v4/authentications` starts the order; polling it ends
//!    with a `gl_order_id`, and `POST /auth/gl/v1/authentications/{id}` finishes it.
//! 4. `/mga/sps/oauth/oauth20/authorize` with the login page's parameters
//!    redirects to `/th/login/v1/end`, which sets `seb-session-id` for *.seb.se.
//!
//! The PKCE exchange happens inside SEB's token handler; the client only
//! carries cookies and follows redirects.

use reqwest::{StatusCode, Url, header::LOCATION};
use serde::Deserialize;
use serde_json::json;

use crate::domain::{ResponseHead, Secret};

use super::{bankid::Progress, browser::Browser};

const CLIENT_ID: &str = "cps";
const HOME: &str = "/cps/home";
const SESSION_COOKIE: &str = "seb-session-id";
/// Redirects to follow from authorize to the session, with room to spare.
const MAX_REDIRECTS: usize = 5;

pub struct Seb {
    /// The login service, id.seb.se.
    id: Url,
    /// The internet bank, apps.seb.se.
    apps: Url,
}

/// A login in progress.
pub struct SebLogin {
    /// Where the browser was sent to log in, with the token handler's parameters.
    login_page: Url,
    order_id: String,
    gl_order_id: Option<String>,
}

#[derive(Deserialize)]
struct Order {
    order_id: String,
    auto_start_token: Option<String>,
}

#[derive(Deserialize)]
struct OrderStatus {
    status: String,
    gl_order_id: Option<String>,
    hint_code: Option<String>,
}

impl Seb {
    /// How the agent uses the internet bank.
    pub const SKILL: &str = include_str!("skills/seb.md");

    pub fn production() -> Self {
        Seb::new(
            Url::parse("https://id.seb.se").expect("a valid URL"),
            Url::parse("https://apps.seb.se").expect("a valid URL"),
        )
    }

    /// SEB at other addresses, for tests.
    pub fn new(id: Url, apps: Url) -> Self {
        Seb { id, apps }
    }

    pub async fn start(&self, browser: &Browser) -> Result<(SebLogin, Secret), String> {
        let mut start = self.at(&self.id, "/th/login/v1/start")?;
        start
            .query_pairs_mut()
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_path", HOME);
        let response = browser.http().get(start.clone()).send().await.map_err(failed)?;
        let login_page = redirect(&start, &response).ok_or_else(|| unexpected("the login start", response.status()))?;

        let page = browser.http().get(login_page.clone()).send().await.map_err(failed)?;
        expect_success("the login page", page.status())?;
        let token = self
            .post(browser, &login_page, "/login/api/login-tokens/v1", None)
            .await?;
        expect_success("the login token", token.status())?;

        let body = json!({ "client_id": CLIENT_ID, "start_mode": "AST", "ui_locales": "sv", "timing_data": "[]" });
        let response = self
            .post(browser, &login_page, "/auth/bid/v4/authentications", Some(body))
            .await?;
        expect_success("starting BankID", response.status())?;
        let order: Order = response.json().await.map_err(|e| format!("starting BankID: {e}"))?;
        let token = order.auto_start_token.ok_or("SEB sent no autostart token")?;
        Ok((
            SebLogin {
                login_page,
                order_id: order.order_id,
                gl_order_id: None,
            },
            Secret::new(token),
        ))
    }

    pub async fn poll(&self, browser: &Browser, login: &mut SebLogin) -> Result<Progress, String> {
        let url = self.at(&self.id, &format!("/auth/bid/v4/authentications/{}", login.order_id))?;
        let response = self
            .request(browser.http().get(url), &login.login_page)
            .send()
            .await
            .map_err(failed)?;
        expect_success("polling BankID", response.status())?;
        let status: OrderStatus = response.json().await.map_err(|e| format!("polling BankID: {e}"))?;
        Ok(match status.status.as_str() {
            "PENDING" => Progress::Pending,
            "COMPLETE" => {
                login.gl_order_id = status.gl_order_id;
                Progress::Complete
            }
            other => Progress::Failed(status.hint_code.unwrap_or_else(|| other.to_owned())),
        })
    }

    pub async fn finish(&self, browser: &Browser, login: SebLogin) -> Result<(), String> {
        let gl_order_id = login
            .gl_order_id
            .as_deref()
            .ok_or("BankID completed without an order to finish")?;
        let path = format!("/auth/gl/v1/authentications/{gl_order_id}");
        let response = self.post(browser, &login.login_page, &path, Some(json!({}))).await?;
        expect_success("finishing the login", response.status())?;

        // The login page's parameters, sent to authorize: SEB redirects to its
        // token handler, which sets the session cookie, and on to the bank's
        // home page, which sets the bank's own cookies. Follow it all, as a
        // browser would.
        let mut next = self.at(&self.id, "/mga/sps/oauth/oauth20/authorize")?;
        next.set_query(login.login_page.query());
        for _ in 0..MAX_REDIRECTS {
            let response = browser.http().get(next.clone()).send().await.map_err(failed)?;
            match redirect(&next, &response) {
                Some(location) => next = location,
                None => break,
            }
        }
        if !browser.has_cookie(&self.apps, SESSION_COOKIE) {
            return Err("SEB didn't hand over a session".into());
        }
        Ok(())
    }

    pub async fn cancel(&self, browser: &Browser, login: &SebLogin) {
        if let Ok(url) = self.at(&self.id, &format!("/auth/bid/v4/authentications/{}", login.order_id)) {
            let _ = self.request(browser.http().delete(url), &login.login_page).send().await;
        }
    }

    pub fn keepalive(&self) -> Url {
        self.at(&self.apps, "/cps/api/notifications/private")
            .unwrap_or_else(|_| self.apps.clone())
    }

    /// A rejected request, or a redirect back to a login page.
    pub fn signed_out(&self, response: &ResponseHead) -> bool {
        if matches!(response.status, 401 | 403) {
            return true;
        }
        let to_login = response
            .location
            .as_deref()
            .and_then(|location| response.url.join(location).ok())
            .is_some_and(|location| {
                location.host() == self.id.host()
                    && ["/expired-session", "/th/login", "/login"]
                        .iter()
                        .any(|path| location.path().starts_with(path))
            });
        (300..400).contains(&response.status) && to_login
    }

    fn at(&self, base: &Url, path: &str) -> Result<Url, String> {
        base.join(path).map_err(|e| e.to_string())
    }

    /// Headers the login page's own requests carry.
    fn request(&self, request: reqwest::RequestBuilder, login_page: &Url) -> reqwest::RequestBuilder {
        request
            .header("accept", "application/json, text/plain, */*")
            .header("referer", login_page.as_str())
    }

    async fn post(
        &self,
        browser: &Browser,
        login_page: &Url,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response, String> {
        let url = self.at(&self.id, path)?;
        let origin = self.id.origin().ascii_serialization();
        let mut request = self
            .request(browser.http().post(url), login_page)
            .header("origin", origin);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.map_err(failed)
    }
}

/// Where a redirect response leads, resolved against the request's URL.
fn redirect(from: &Url, response: &reqwest::Response) -> Option<Url> {
    if !response.status().is_redirection() {
        return None;
    }
    let location = response.headers().get(LOCATION)?.to_str().ok()?;
    from.join(location).ok()
}

fn expect_success(step: &str, status: StatusCode) -> Result<(), String> {
    if status.is_success() {
        Ok(())
    } else {
        Err(unexpected(step, status))
    }
}

fn unexpected(step: &str, status: StatusCode) -> String {
    format!("{step} answered {status}")
}

fn failed(error: reqwest::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(status: u16, location: Option<&str>) -> ResponseHead {
        ResponseHead {
            url: Url::parse("https://apps.seb.se/ssc/accounts-widget/api/accounts/all").unwrap(),
            status,
            set_cookies: Vec::new(),
            location: location.map(str::to_owned),
        }
    }

    #[test]
    fn a_session_is_over_on_401_403_or_a_redirect_to_login() {
        let seb = Seb::production();
        assert!(seb.signed_out(&head(401, None)));
        assert!(seb.signed_out(&head(403, None)));
        assert!(seb.signed_out(&head(
            302,
            Some("https://id.seb.se/expired-session?redirect_path=/cps/home")
        )));
        assert!(seb.signed_out(&head(302, Some("https://id.seb.se/th/login/v1/start?client_id=cps"))));
        assert!(!seb.signed_out(&head(200, None)));
        assert!(!seb.signed_out(&head(404, None)));
        assert!(!seb.signed_out(&head(302, Some("/cps/home"))));
    }
}
