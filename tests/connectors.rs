use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use base64::Engine;
use evalien::connectors::config::{self, CallbackConfig, ConnectionConfig, GatewayConfig, KindConfig, VaultConfig};
use evalien::connectors::{Connectors, OAuthClient, Vault, VaultKey, serve_callback};
use evalien::db::MemoryStore;
use evalien::domain::{
    Attachment, ConnectionStatus, Event, Gateway, Origin, RequestOutcome, ResponseHead, Secret, Seq, Timestamp,
    Trajectory, Usd,
};
use evalien::evaluation::{Capabilities, Limits, Sandbox, SandboxHost};
use evalien::metrics::Metrics;
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const API_KEYS: &str = r#"
    [connections.weather]
    kind = "api_key"
    origins = ["https://api.weather.example", "https://maps.weather.example:8443"]
    header = { name = "X-Api-Key", value = "{key}" }
    key = { secret = "WEATHER_KEY" }

    [connections.notes]
    kind = "api_key"
    origins = ["https://notes.example"]
    header = { name = "Authorization", value = "Bearer {key}" }
    key = { secret = "NOTES_KEY" }
"#;

fn log() -> Arc<Trajectory> {
    Arc::new(Trajectory::new(MemoryStore::new()).unwrap())
}

fn api_keys() -> Arc<Connectors> {
    let env: HashMap<String, String> = [("WEATHER_KEY", "w-secret-1"), ("NOTES_KEY", "n-secret-2")]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    let config = GatewayConfig {
        connections: config::parse(API_KEYS, &env).unwrap(),
        ..GatewayConfig::default()
    };
    Connectors::new(config, log(), |_| {}).unwrap()
}

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn status(n: u16) -> RequestOutcome {
    RequestOutcome::Response(ResponseHead::status(url("https://example.com/"), n))
}

#[test]
fn credentials_go_only_to_the_exact_origins_of_their_connection() {
    let gateway = api_keys();
    match gateway.attach(&url("https://api.weather.example/v1/forecast?city=sthlm")) {
        Attachment::Credentials { connection, headers } => {
            assert_eq!(connection, "weather");
            assert_eq!(
                (headers[0].0.as_str(), headers[0].1.expose()),
                ("X-Api-Key", "w-secret-1")
            );
        }
        other => panic!("expected credentials, got {other:?}"),
    }
    assert!(matches!(
        gateway.attach(&url("https://maps.weather.example:8443/tiles")),
        Attachment::Credentials { .. }
    ));
    for elsewhere in [
        "http://api.weather.example/",       // plain http
        "https://maps.weather.example/",     // another port
        "https://evil.api.weather.example/", // a subdomain
        "https://api.weather.example.evil/", // a lookalike
        "https://example.com/",
    ] {
        assert_eq!(gateway.attach(&url(elsewhere)), Attachment::Nothing, "{elsewhere}");
    }
}

#[test]
fn outcomes_are_counted_per_connection_and_an_api_key_stays_connected() {
    let gateway = api_keys();
    for outcome in [
        status(200),
        status(204),
        status(401),
        status(404),
        status(503),
        RequestOutcome::Failed,
    ] {
        assert_eq!(gateway.observe("weather", outcome), ConnectionStatus::Connected);
    }
    let weather = gateway.connections().into_iter().find(|c| c.name == "weather").unwrap();
    let r = weather.requests;
    assert_eq!(
        (r.succeeded, r.rejected, r.client_errors, r.server_errors, r.failed),
        (2, 1, 1, 1, 1)
    );
}

#[tokio::test]
async fn the_agent_sees_its_connections_but_never_their_credentials() {
    let gateway: Arc<dyn Gateway> = api_keys();
    let host = SandboxHost::new().unwrap();
    let mut sandbox = Sandbox::new(host, Capabilities::none().with_connections(gateway), Limits::default())
        .await
        .unwrap();
    sandbox
        .evaluate(Seq::new(1), "return JSON.stringify(await connections.list())")
        .await;
    let mut events = sandbox.take_events();
    while sandbox.is_waiting() {
        let wakeup = sandbox.next_wakeup().await.unwrap();
        sandbox.wake(wakeup).await;
        events.extend(sandbox.take_events());
    }
    let listed = events
        .iter()
        .find_map(|event| match event {
            Event::Result { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(!listed.contains("secret"), "a credential leaked: {listed}");
    let json: String = serde_json::from_str(&listed).unwrap();
    let connections: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        connections,
        serde_json::json!([
            { "name": "notes", "kind": "api_key", "status": "connected", "origins": ["https://notes.example"], "skill": false },
            { "name": "weather", "kind": "api_key", "status": "connected",
              "origins": ["https://api.weather.example", "https://maps.weather.example:8443"], "skill": false },
        ])
    );
}

#[test]
fn metrics_report_requests_and_status_per_connection() {
    let gateway: Arc<dyn Gateway> = api_keys();
    gateway.observe("weather", status(200));
    gateway.observe("weather", status(401));
    let metrics = Metrics::new(Usd::new(1.0).unwrap(), Usd::ZERO, Timestamp::now()).with_gateway(gateway);
    let text = metrics.render(Timestamp::now());
    assert!(
        text.contains(r#"evalien_connection_requests_total{connection="weather",outcome="succeeded"} 1"#),
        "{text}"
    );
    assert!(text.contains(r#"evalien_connection_requests_total{connection="weather",outcome="rejected"} 1"#));
    assert!(text.contains(r#"evalien_connection_connected{connection="notes",kind="api_key"} 1"#));
    assert!(!text.contains("secret"));
}

/// A fake OAuth provider's token endpoint. Each code exchange issues
/// refresh token rt-1; refreshes issue at-2, at-3 …, or refuse when told to.
#[derive(Default)]
struct Provider {
    requests: Mutex<Vec<HashMap<String, String>>>,
    refuse: AtomicBool,
    issued: AtomicU32,
}

async fn provider() -> (Url, Arc<Provider>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let state = Arc::new(Provider::default());
    let shared = state.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(token_endpoint(stream, shared.clone()));
        }
    });
    (base, state)
}

async fn token_endpoint(mut stream: TcpStream, provider: Arc<Provider>) {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let body_start = loop {
        let n = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap());
    while request.len() < body_start + length {
        let n = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..n]);
    }
    let form: HashMap<String, String> = url::form_urlencoded::parse(&request[body_start..])
        .into_owned()
        .collect();
    provider.requests.lock().unwrap().push(form.clone());
    let (status, body) = match form["grant_type"].as_str() {
        "authorization_code" => (
            "200 OK",
            r#"{"access_token":"at-1","expires_in":3600,"refresh_token":"rt-1","token_type":"Bearer"}"#.to_owned(),
        ),
        _ if provider.refuse.load(Ordering::SeqCst) => (
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#.to_owned(),
        ),
        _ => {
            let n = provider.issued.fetch_add(1, Ordering::SeqCst) + 2;
            (
                "200 OK",
                format!(r#"{{"access_token":"at-{n}","expires_in":3600,"token_type":"Bearer"}}"#),
            )
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

const REDIRECT: &str = "https://evalien.test/oauth/callback";

fn vault_key() -> VaultKey {
    VaultKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap()
}

fn oauth(base: &Url, vault: &std::path::Path, log: Arc<Trajectory>) -> Arc<Connectors> {
    let client = OAuthClient::new(
        base.join("authorize").unwrap(),
        base.join("token").unwrap(),
        "client-1".into(),
        Secret::new("client-secret"),
        vec!["calendar".into()],
    );
    let config = GatewayConfig {
        connections: vec![ConnectionConfig {
            name: "calendar".into(),
            origins: vec![Origin::parse("https://www.googleapis.com").unwrap()],
            kind: KindConfig::OAuth {
                client: Box::new(client),
            },
        }],
        vault: Some(VaultConfig {
            path: vault.to_owned(),
            key: vault_key(),
        }),
        callback: Some(CallbackConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            redirect: Url::parse(REDIRECT).unwrap(),
        }),
    };
    Connectors::new(config, log, |_| {}).unwrap()
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

/// The sign-in link from the latest notice, as query parameters.
fn sign_in_link(log: &Trajectory) -> HashMap<String, String> {
    let notice = notices(log)
        .into_iter()
        .rev()
        .find(|n| n.starts_with("Sign in to calendar"))
        .expect("no sign-in notice");
    let url = Url::parse(notice.rsplit(": ").next().unwrap()).unwrap();
    url.query_pairs().into_owned().collect()
}

fn bearer(gateway: &Connectors) -> Option<String> {
    match gateway.attach(&url("https://www.googleapis.com/calendar/v3/users/me/calendarList")) {
        Attachment::Credentials { headers, .. } => Some(headers[0].1.expose().to_owned()),
        _ => None,
    }
}

async fn until(condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn an_oauth_login_runs_from_connect_to_signed_requests() {
    let (base, provider) = provider().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = oauth(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;
    assert_eq!(
        gateway.attach(&url("https://www.googleapis.com/")),
        Attachment::NotConnected {
            connection: "calendar".into(),
            status: ConnectionStatus::NotConnected
        }
    );

    // Two connects at once share one login: one link for the human.
    let first = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.connect("calendar", Duration::from_secs(5)).await }
    });
    let second = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.connect("calendar", Duration::from_secs(5)).await }
    });
    until(|| gateway.connections()[0].status == ConnectionStatus::LoggingIn).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(notices(&log).len(), 1);

    let link = sign_in_link(&log);
    assert_eq!(link["redirect_uri"], REDIRECT);
    assert_eq!(link["code_challenge_method"], "S256");
    assert_eq!(
        gateway.complete(&link["state"], "code-from-google").await.unwrap(),
        "calendar"
    );
    assert_eq!(first.await.unwrap(), Ok(ConnectionStatus::Connected));
    assert_eq!(second.await.unwrap(), Ok(ConnectionStatus::Connected));
    assert_eq!(bearer(&gateway).as_deref(), Some("Bearer at-1"));
    assert!(notices(&log).last().unwrap().contains("calendar is connected"));

    // The provider got the verifier that hashes to the link's challenge.
    let exchange = provider.requests.lock().unwrap()[0].clone();
    assert_eq!(exchange["code"], "code-from-google");
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(exchange["code_verifier"].as_bytes()));
    assert_eq!(challenge, link["code_challenge"]);

    // The refresh token is in the vault, encrypted.
    let raw = std::fs::read(dir.path().join("vault.db")).unwrap();
    assert!(!raw.windows(4).any(|w| w == b"rt-1"));
    let vault = Vault::open(&dir.path().join("vault.db"), &vault_key()).unwrap();
    assert_eq!(
        vault.get("calendar", "refresh_token").unwrap().unwrap().expose(),
        "rt-1"
    );

    // A link can't be used twice.
    assert!(
        gateway
            .complete(&link["state"], "again")
            .await
            .unwrap_err()
            .contains("expired or was already used")
    );
}

#[tokio::test]
async fn a_restart_refreshes_from_the_vault_and_a_revoked_grant_asks_for_a_new_login() {
    let (base, provider) = provider().await;
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault.db");
    Vault::open(&vault, &vault_key())
        .unwrap()
        .put("calendar", "refresh_token", &Secret::new("rt-1"))
        .unwrap();

    let log = log();
    let gateway = oauth(&base, &vault, log.clone());
    gateway.start().await;
    assert_eq!(gateway.connections()[0].status, ConnectionStatus::Connected);
    assert_eq!(bearer(&gateway).as_deref(), Some("Bearer at-2"));
    assert_eq!(provider.requests.lock().unwrap()[0]["refresh_token"], "rt-1");

    // The grant is revoked: a 401 prompts a refresh, which is refused.
    provider.refuse.store(true, Ordering::SeqCst);
    gateway.observe("calendar", status(401));
    until(|| gateway.connections()[0].status == ConnectionStatus::Expired).await;
    assert!(
        notices(&log)
            .last()
            .unwrap()
            .contains("calendar needs signing in again")
    );
    assert_eq!(
        gateway.attach(&url("https://www.googleapis.com/")),
        Attachment::NotConnected {
            connection: "calendar".into(),
            status: ConnectionStatus::Expired
        }
    );
    assert!(
        Vault::open(&vault, &vault_key())
            .unwrap()
            .get("calendar", "refresh_token")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_declined_or_abandoned_login_leaves_the_connection_as_it_was() {
    let (base, _) = provider().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = oauth(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;

    let declined = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.connect("calendar", Duration::from_secs(5)).await }
    });
    until(|| gateway.connections()[0].status == ConnectionStatus::LoggingIn).await;
    let state = sign_in_link(&log)["state"].clone();
    assert!(
        gateway
            .refuse(&state, "access_denied")
            .unwrap_err()
            .contains("access_denied")
    );
    assert!(declined.await.unwrap().unwrap_err().contains("access_denied"));
    assert_eq!(gateway.connections()[0].status, ConnectionStatus::NotConnected);

    let timed_out = gateway.connect("calendar", Duration::from_millis(50)).await;
    assert!(timed_out.unwrap_err().contains("no sign-in within"));
    assert_eq!(gateway.connections()[0].status, ConnectionStatus::NotConnected);
    let late = sign_in_link(&log)["state"].clone();
    assert!(gateway.complete(&late, "too-late").await.is_err());
}

#[tokio::test]
async fn the_callback_endpoint_finishes_the_login_in_the_browser() {
    let (base, _) = provider().await;
    let dir = tempfile::tempdir().unwrap();
    let log = log();
    let gateway = oauth(&base, &dir.path().join("vault.db"), log.clone());
    gateway.start().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_callback(listener, gateway.clone(), |_| {}));

    let connecting = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.connect("calendar", Duration::from_secs(5)).await }
    });
    until(|| gateway.connections()[0].status == ConnectionStatus::LoggingIn).await;
    let state = sign_in_link(&log)["state"].clone();

    let get = |target: String| async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nhost: evalien\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    };
    let forged = get("/oauth/callback?state=forged&code=x".into()).await;
    assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
    let page = get(format!("/oauth/callback?state={state}&code=real-code")).await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("Signed in to calendar"));
    assert!(page.contains("cache-control: no-store"));
    assert_eq!(connecting.await.unwrap(), Ok(ConnectionStatus::Connected));
    assert!(get("/elsewhere".into()).await.starts_with("HTTP/1.1 404"));
}
