use std::{collections::HashMap, sync::Arc};

use evalien::connectors::{Connectors, config};
use evalien::domain::{Attachment, ConnectionStatus, Gateway, Origin, RequestOutcome, Timestamp, Usd};
use evalien::evaluation::{Capabilities, Limits, Sandbox, SandboxHost};
use evalien::metrics::Metrics;

const CONFIG: &str = r#"
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

fn gateway() -> Connectors {
    let env: HashMap<String, String> = [("WEATHER_KEY", "w-secret-1"), ("NOTES_KEY", "n-secret-2")]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    Connectors::new(config::parse(CONFIG, &env).unwrap())
}

fn origin(url: &str) -> Origin {
    Origin::of(&url::Url::parse(url).unwrap())
}

#[test]
fn credentials_go_only_to_the_exact_origins_of_their_connection() {
    let gateway = gateway();
    match gateway.attach(&origin("https://api.weather.example/v1/forecast?city=sthlm")) {
        Attachment::Credentials { connection, headers } => {
            assert_eq!(connection, "weather");
            assert_eq!(headers[0].0, "X-Api-Key");
            assert_eq!(headers[0].1.expose(), "w-secret-1");
        }
        other => panic!("expected credentials, got {other:?}"),
    }
    assert!(matches!(
        gateway.attach(&origin("https://maps.weather.example:8443/tiles")),
        Attachment::Credentials { .. }
    ));
    for elsewhere in [
        "http://api.weather.example/",       // plain http
        "https://maps.weather.example/",     // another port
        "https://evil.api.weather.example/", // a subdomain
        "https://api.weather.example.evil/", // a lookalike
        "https://example.com/",
    ] {
        assert_eq!(gateway.attach(&origin(elsewhere)), Attachment::Nothing, "{elsewhere}");
    }
}

#[test]
fn outcomes_are_counted_per_connection_and_an_api_key_stays_connected() {
    let gateway = gateway();
    for outcome in [
        RequestOutcome::Status(200),
        RequestOutcome::Status(204),
        RequestOutcome::Status(401),
        RequestOutcome::Status(404),
        RequestOutcome::Status(503),
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
    let notes = gateway.connections().into_iter().find(|c| c.name == "notes").unwrap();
    assert_eq!(notes.requests.succeeded, 0);
}

#[tokio::test]
async fn the_agent_sees_its_connections_but_never_their_credentials() {
    let gateway: Arc<dyn Gateway> = Arc::new(gateway());
    let host = SandboxHost::new().unwrap();
    let mut sandbox = Sandbox::new(host, Capabilities::none().with_connections(gateway), Limits::default())
        .await
        .unwrap();
    let eval = evalien::domain::Seq::new(1);
    sandbox
        .evaluate(eval, "return JSON.stringify(await connections.list())")
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
            evalien::domain::Event::Result { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    // The result is the inspected string: decode it, then the JSON inside.
    let json: String = serde_json::from_str(&listed).unwrap();
    let connections: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        connections,
        serde_json::json!([
            { "name": "notes", "kind": "api_key", "status": "connected", "origins": ["https://notes.example"] },
            { "name": "weather", "kind": "api_key", "status": "connected",
              "origins": ["https://api.weather.example", "https://maps.weather.example:8443"] },
        ])
    );
    assert!(!listed.contains("secret"), "a credential leaked: {listed}");
}

#[test]
fn metrics_report_requests_and_status_per_connection() {
    let gateway: Arc<dyn Gateway> = Arc::new(gateway());
    gateway.observe("weather", RequestOutcome::Status(200));
    gateway.observe("weather", RequestOutcome::Status(401));
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
