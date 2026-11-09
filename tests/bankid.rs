//! The BankID connection against a fake SEB that enforces the real flow's
//! handoffs: the login token cookie before an order, the login page's state
//! and challenge at authorize, and the session cookie on the bank's APIs.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use base64::Engine;
use evalien::connectors::config::{ConnectionConfig, GatewayConfig, KindConfig, VaultConfig};
use evalien::connectors::{BankIdProvider, Connectors, Seb, Vault, VaultKey};
use evalien::db::MemoryStore;
use evalien::domain::{
    Attachment, ConnectionStatus, Event, Gateway, Origin, Outcome, RequestOutcome, ResponseHead, Seq, Trajectory,
};
use evalien::evaluation::{Capabilities, Limits, Sandbox, SandboxHost};
use reqwest::Url;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const STATE: &str = "token-handler-state";
const CHALLENGE: &str = "pkce-challenge";

/// What the fake SEB knows and has seen.
#[derive(Default)]
struct Bank {
    approved: AtomicBool,
    declined: AtomicBool,
    /// Whether the session cookie is still accepted.
    revoked: AtomicBool,
    cancelled: AtomicBool,
    /// Every request, as "METHOD /path".
    seen: Mutex<Vec<String>>,
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    cookies: HashMap<String, String>,
}

async fn fake_seb() -> (Url, Arc<Bank>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let bank = Arc::new(Bank::default());
    let shared = bank.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(stream, shared.clone()));
        }
    });
    (base, bank)
}

async fn serve(mut stream: TcpStream, bank: Arc<Bank>) {
    let mut raw = Vec::new();
    let mut buffer = [0; 8192];
    let body_start = loop {
        let n = stream.read(&mut buffer).await.unwrap();
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&buffer[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..body_start]).into_owned();
    let header = |name: &str| {
        head.lines()
            .find(|line| line.to_lowercase().starts_with(&format!("{name}:")))
            .map(|line| line[name.len() + 1..].trim().to_owned())
            .unwrap_or_default()
    };
    let length: usize = header("content-length").parse().unwrap_or(0);
    while raw.len() < body_start + length {
        let n = stream.read(&mut buffer).await.unwrap();
        raw.extend_from_slice(&buffer[..n]);
    }
    let mut first = head.lines().next().unwrap().split(' ');
    let (method, target) = (first.next().unwrap().to_owned(), first.next().unwrap().to_owned());
    let url = Url::parse(&format!("http://seb{target}")).unwrap();
    let request = Request {
        method,
        path: url.path().to_owned(),
        query: url.query_pairs().into_owned().collect(),
        cookies: header("cookie")
            .split(';')
            .filter_map(|pair| pair.trim().split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .collect(),
    };
    bank.seen
        .lock()
        .unwrap()
        .push(format!("{} {}", request.method, request.path));
    let response = route(&request, &bank);
    let _ = stream.write_all(response.as_bytes()).await;
}

fn reply(status: &str, headers: &[String], body: &str) -> String {
    let headers: String = headers.iter().map(|h| format!("{h}\r\n")).collect();
    format!(
        "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn route(request: &Request, bank: &Bank) -> String {
    let json = |body: &str| reply("200 OK", &["content-type: application/json".into()], body);
    let redirect = |to: &str, cookies: &[&str]| {
        let mut headers = vec![format!("location: {to}")];
        headers.extend(cookies.iter().map(|c| format!("set-cookie: {c}")));
        reply("302 Found", &headers, "")
    };
    let signed_in = request.cookies.get("seb-session-id").map(String::as_str) == Some("session-1")
        && !bank.revoked.load(Ordering::SeqCst);
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/th/login/v1/start") => redirect(
            &format!(
                "/login/?client_id=cps&redirect_path=%2Fcps%2Fhome&state={STATE}&redirect_uri=x&response_type=code&code_challenge={CHALLENGE}&code_challenge_method=S256"
            ),
            &["th-pool=1; Path=/"],
        ),
        ("GET", "/login/") => reply("200 OK", &["set-cookie: login-pool=1; Path=/".into()], "<html></html>"),
        ("POST", "/login/api/login-tokens/v1") => reply(
            "200 OK",
            &["set-cookie: seb-login-token=lt-1; Path=/; HttpOnly".into()],
            r#"{"payload":"p"}"#,
        ),
        ("POST", "/auth/bid/v4/authentications") if request.cookies.contains_key("seb-login-token") => json(
            r#"{"order_id":"o-1","auto_start_token":"ast-1","status":"PENDING","hint_code":"OUTSTANDING_TRANSACTION"}"#,
        ),
        ("POST", "/auth/bid/v4/authentications") => reply("403 Forbidden", &[], ""),
        ("GET", "/auth/bid/v4/authentications/o-1") if bank.declined.load(Ordering::SeqCst) => {
            json(r#"{"order_id":"o-1","status":"FAILED","hint_code":"userCancel"}"#)
        }
        ("GET", "/auth/bid/v4/authentications/o-1") if bank.approved.load(Ordering::SeqCst) => {
            json(r#"{"order_id":"o-1","status":"COMPLETE","gl_order_id":"g-1"}"#)
        }
        ("GET", "/auth/bid/v4/authentications/o-1") => json(
            r#"{"order_id":"o-1","auto_start_token":"ast-1","status":"PENDING","hint_code":"OUTSTANDING_TRANSACTION"}"#,
        ),
        ("DELETE", "/auth/bid/v4/authentications/o-1") => {
            bank.cancelled.store(true, Ordering::SeqCst);
            reply("204 No Content", &[], "")
        }
        ("POST", "/auth/gl/v1/authentications/g-1") => json(r#"{"status":"COMPLETE"}"#),
        ("GET", "/mga/sps/oauth/oauth20/authorize")
            if request.query.get("state").map(String::as_str) == Some(STATE)
                && request.query.get("code_challenge").map(String::as_str) == Some(CHALLENGE) =>
        {
            redirect(&format!("/th/login/v1/end?state={STATE}&code=code-1&iss=x"), &[])
        }
        ("GET", "/th/login/v1/end") if request.query.get("code").map(String::as_str) == Some("code-1") => {
            redirect("/cps/home", &["seb-session-id=session-1; Path=/; HttpOnly"])
        }
        ("GET", "/cps/api/notifications/private") if signed_in => json("[]"),
        ("GET", "/cps/api/notifications/private") => redirect("/expired-session?redirect_path=/cps/home", &[]),
        ("GET", "/ssc/accounts") if signed_in => reply(
            "200 OK",
            &["set-cookie: ts=rotated; Path=/".into()],
            r#"{"accounts":[]}"#,
        ),
        _ => reply("401 Unauthorized", &[], ""),
    }
}

fn vault_key() -> VaultKey {
    VaultKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

fn gateway(base: &Url, vault: &std::path::Path, log: Arc<Trajectory>) -> Arc<Connectors> {
    let seb = Seb::new(base.clone(), base.clone()).with_timing(Duration::from_millis(10), Duration::from_millis(500));
    let config = GatewayConfig {
        connections: vec![ConnectionConfig {
            name: "seb".into(),
            origins: vec![Origin::of(base)],
            kind: KindConfig::BankId {
                provider: Box::new(BankIdProvider::Seb(seb)),
                keepalive_for: Duration::from_secs(3600),
            },
        }],
        vault: Some(VaultConfig {
            path: vault.to_owned(),
            key: vault_key(),
        }),
        callback: None,
    };
    Connectors::new(config, log, |_| {}).unwrap()
}

fn log() -> Arc<Trajectory> {
    Arc::new(Trajectory::new(MemoryStore::new()).unwrap())
}

fn notices(log: &Trajectory) -> Vec<String> {
    log.read_all()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.event {
            Event::Notice { text } => Some(text),
            _ => None,
        })
        .collect()
}

fn cookies(gateway: &Connectors, url: &Url) -> Option<String> {
    match gateway.attach(url) {
        Attachment::Credentials { headers, .. } => headers.first().map(|(_, value)| value.expose().to_owned()),
        _ => None,
    }
}

fn status(gateway: &Connectors) -> ConnectionStatus {
    gateway.connections()[0].status
}

async fn until(condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Log in: the human taps the link and approves.
async fn log_in(gateway: &Arc<Connectors>, bank: &Bank, log: &Trajectory) {
    let connecting = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.connect("seb", Duration::from_secs(5)).await }
    });
    until(|| notices(log).iter().any(|n| n.contains("autostarttoken=ast-1"))).await;
    bank.approved.store(true, Ordering::SeqCst);
    assert_eq!(connecting.await.unwrap(), Ok(ConnectionStatus::Connected));
}

#[tokio::test]
async fn a_bankid_login_hands_the_bank_session_to_the_agents_requests() {
    let (base, bank) = fake_seb().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = gateway(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;
    let accounts = base.join("/ssc/accounts").unwrap();
    assert_eq!(
        gateway.attach(&accounts),
        Attachment::NotConnected {
            connection: "seb".into(),
            status: ConnectionStatus::NotConnected
        }
    );

    log_in(&gateway, &bank, &log).await;
    let link = notices(&log)
        .into_iter()
        .find(|n| n.contains("app.bankid.com"))
        .unwrap();
    assert!(
        link.ends_with("https://app.bankid.com/?autostarttoken=ast-1&redirect=null"),
        "{link}"
    );
    assert!(notices(&log).last().unwrap().contains("seb is connected"));

    // The flow ran in order, and the bank's session reaches the agent's requests.
    let seen = bank.seen.lock().unwrap().clone();
    let steps: Vec<&str> = seen
        .iter()
        .map(String::as_str)
        .filter(|s| !s.ends_with("/o-1"))
        .collect();
    assert_eq!(
        steps,
        [
            "GET /th/login/v1/start",
            "GET /login/",
            "POST /login/api/login-tokens/v1",
            "POST /auth/bid/v4/authentications",
            "POST /auth/gl/v1/authentications/g-1",
            "GET /mga/sps/oauth/oauth20/authorize",
            "GET /th/login/v1/end",
            "GET /cps/home",
        ]
    );
    let sent = cookies(&gateway, &accounts).unwrap();
    assert!(sent.contains("seb-session-id=session-1"), "{sent}");

    // The session is in the vault, encrypted.
    let raw = std::fs::read(dir.path().join("vault.db")).unwrap();
    assert!(!raw.windows(9).any(|w| w == b"session-1"));
    assert!(
        Vault::open(&dir.path().join("vault.db"), &vault_key())
            .unwrap()
            .get("seb", "cookies")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn cookies_the_bank_sets_on_the_agents_requests_are_kept() {
    let (base, bank) = fake_seb().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = gateway(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;
    log_in(&gateway, &bank, &log).await;

    let accounts = base.join("/ssc/accounts").unwrap();
    let response = ResponseHead {
        url: accounts.clone(),
        status: 200,
        set_cookies: vec!["ts=rotated; Path=/".into()],
        location: None,
    };
    assert_eq!(
        gateway.observe("seb", RequestOutcome::Response(response)),
        ConnectionStatus::Connected
    );
    assert!(cookies(&gateway, &accounts).unwrap().contains("ts=rotated"));
}

#[tokio::test]
async fn a_restart_restores_the_session_or_notices_it_has_ended() {
    let (base, bank) = fake_seb().await;
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault.db");
    let first = gateway(&base, &vault, log());
    first.start().await;
    let log_one = log();
    let signed_in = gateway(&base, &vault, log_one.clone());
    signed_in.start().await;
    log_in(&signed_in, &bank, &log_one).await;
    drop(first);

    // Same vault, still signed in at the bank: connected without a login.
    let restarted = gateway(&base, &vault, log());
    restarted.start().await;
    assert_eq!(status(&restarted), ConnectionStatus::Connected);

    // The bank ended the session meanwhile: the check on start notices.
    bank.revoked.store(true, Ordering::SeqCst);
    let log_two = log();
    let later = gateway(&base, &vault, log_two.clone());
    later.start().await;
    assert_eq!(status(&later), ConnectionStatus::Expired);
    assert!(notices(&log_two).iter().any(|n| n.contains("needs signing in again")));
}

#[tokio::test]
async fn a_rejected_request_ends_the_session_and_clears_it() {
    let (base, bank) = fake_seb().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = gateway(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;
    log_in(&gateway, &bank, &log).await;

    let redirected = ResponseHead {
        url: base.join("/ssc/accounts").unwrap(),
        status: 302,
        set_cookies: Vec::new(),
        location: Some("/expired-session?redirect_path=/cps/home".into()),
    };
    assert_eq!(
        gateway.observe("seb", RequestOutcome::Response(redirected)),
        ConnectionStatus::Expired
    );
    assert!(notices(&log).last().unwrap().contains("needs signing in again"));
    assert!(
        Vault::open(&dir.path().join("vault.db"), &vault_key())
            .unwrap()
            .get("seb", "cookies")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_declined_or_unanswered_login_leaves_the_connection_as_it_was() {
    let (base, bank) = fake_seb().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = gateway(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;

    bank.declined.store(true, Ordering::SeqCst);
    let declined = gateway.connect("seb", Duration::from_secs(5)).await;
    assert!(declined.unwrap_err().contains("userCancel"));
    assert_eq!(status(&gateway), ConnectionStatus::NotConnected);

    // Nobody approves: the order is withdrawn when its lifetime runs out.
    bank.declined.store(false, Ordering::SeqCst);
    let unanswered = gateway.connect("seb", Duration::from_secs(5)).await;
    assert!(unanswered.unwrap_err().contains("in time"));
    assert!(bank.cancelled.load(Ordering::SeqCst));
    assert_eq!(status(&gateway), ConnectionStatus::NotConnected);
}

#[tokio::test]
async fn sebs_skill_tells_the_agent_how_and_its_code_works() {
    let (base, _) = fake_seb().await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = gateway(&base, &dir.path().join("vault.db"), log());
    assert!(gateway.connections()[0].skill);
    let skill = gateway.skill("seb").unwrap();
    assert!(skill.contains("accounts-widget/api/accounts/all"), "{skill}");
    assert!(gateway.skill("nope").is_none());

    // The skill's snippet, run in the sandbox against HTML shaped like SEB's home page.
    let snippet = skill.split("```js\n").nth(1).unwrap().split("```").next().unwrap();
    let page = r#"<script>window.$$Shell.user = window.$$Shell.user || {"id":"12345678901234","name":"A PERSON","friendlyId":"19800101-0000","sebCustNo":"12345678901234"}
window.$$Shell.principals = [];</script>"#;
    let code = format!(
        "const fetch = async () => ({{ text: async () => {} }});\n{snippet}\nreturn user.id + ' ' + user.sebCustNo;",
        serde_json::to_string(page).unwrap()
    );
    let mut sandbox = Sandbox::new(SandboxHost::new().unwrap(), Capabilities::none(), Limits::default())
        .await
        .unwrap();
    sandbox.evaluate(Seq::new(1), &code).await;
    let result = sandbox.take_events().into_iter().find_map(|event| match event {
        Event::Result { outcome, text, .. } => Some((outcome, text)),
        _ => None,
    });
    assert_eq!(
        result,
        Some((Outcome::Return, r#""12345678901234 12345678901234""#.to_owned()))
    );
}
