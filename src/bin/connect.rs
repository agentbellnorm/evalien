//! Try a connection from the terminal, without the agent: sign in, then call
//! the service with the agent's own fetch, through the gateway.
//!
//! usage: connect <connections.toml> <name> [url] [--show]
//!
//! BankID links are shown as a QR code: scan it with the phone's camera to
//! open BankID. Responses are summarized as their status and JSON shape;
//! `--show` prints the body too. Without VAULT_KEY, the session lives in a
//! temporary vault for this run only.

use std::{process::ExitCode, sync::Arc, time::Duration};

use base64::Engine;
use evalien::{
    config,
    connectors::{self, Connectors, Vault},
    db::MemoryStore,
    domain::{Event, Gateway, Seq, Trajectory},
    evaluation::capabilities::{CONNECTION_HEADER, Fetch},
};
use qrcode::{QrCode, render::unicode::Dense1x2};
use serde_json::{Value, json};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show = args.iter().any(|arg| arg == "--show");
    let positional: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    let (Some(file), Some(name)) = (positional.first(), positional.get(1)) else {
        eprintln!("usage: connect <connections.toml> <name> [url] [--show]");
        return ExitCode::from(64);
    };
    let url = positional.get(2).map(|url| url.as_str());
    match run(file, name, url, show).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("connect: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(file: &str, name: &str, url: Option<&str>, show: bool) -> Result<(), String> {
    let mut env = config::environment();
    env.insert("CONNECTIONS_FILE".into(), file.to_owned());
    // Without a vault key, a temporary vault whose key exists only in memory.
    let temporary = std::env::temp_dir().join(format!("evalien-connect-{}.db", std::process::id()));
    if !env.contains_key("VAULT_KEY") {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| e.to_string())?;
        env.insert(
            "VAULT_KEY".into(),
            base64::engine::general_purpose::STANDARD.encode(key),
        );
        env.insert("VAULT_PATH".into(), temporary.display().to_string());
    }
    let result = connect(&env, name, url, show).await;
    let _ = std::fs::remove_file(&temporary);
    result
}

async fn connect(
    env: &std::collections::HashMap<String, String>,
    name: &str,
    url: Option<&str>,
    show: bool,
) -> Result<(), String> {
    let gateway_config = connectors::config::read(env, std::path::Path::new("."))?;
    let redirect = gateway_config.callback.as_ref().map(|callback| &callback.redirect);
    let services = connectors::providers::resolve(gateway_config.connections, redirect)?;
    let vault = match &gateway_config.vault {
        Some(vault) => Some(Arc::new(
            Vault::open(&vault.path, &vault.key).map_err(|e| e.to_string())?,
        )),
        None => None,
    };
    let log = Arc::new(Trajectory::new(MemoryStore::new()).map_err(|e| e.to_string())?);
    tokio::spawn(show_notices(log.clone()));
    let deps = connectors::Deps {
        vault,
        http: reqwest::Client::new(),
        log,
        debug: |message| eprintln!("{message}"),
    };
    let gateway = Connectors::start(services, deps).await?;

    eprintln!("connecting {name}…");
    let status = gateway.connect(name, LOGIN_TIMEOUT).await?;
    eprintln!("{name}: {}", status.as_str());
    if let Some(skill) = gateway.skill(name) {
        eprintln!(
            "({name} has a skill of {} lines; the agent reads it with connections.skill)",
            skill.lines().count()
        );
    }

    let Some(url) = url else { return Ok(()) };
    // The agent's own fetch, so the request goes out as the agent's would.
    let fetch = Fetch::new(Some(gateway)).map_err(|e| e.to_string())?;
    let request = json!({ "url": url, "method": "GET", "headers": { "accept": "application/json, text/html" } });
    let response = fetch
        .request(serde_json::from_value(request).map_err(|e| e.to_string())?)
        .await?;
    let hint = response["headers"][CONNECTION_HEADER].as_str().unwrap_or("ok");
    println!("{} {} (connection: {hint})", response["status"], response["url"]);
    let body = response["body"].as_str().unwrap_or_default();
    match serde_json::from_str::<Value>(body) {
        Ok(json) => println!(
            "{}",
            serde_json::to_string_pretty(&if show { json } else { shape(&json) }).unwrap_or_default()
        ),
        Err(_) if show => println!("{body}"),
        Err(_) => println!("<{} bytes, not JSON; --show prints it>", body.len()),
    }
    Ok(())
}

/// A value's structure, with every string, number and bool replaced by its type.
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(fields.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        Value::Array(items) => Value::Array(items.first().map(shape).into_iter().collect()),
        Value::String(_) => Value::String("string".into()),
        Value::Number(_) => Value::String("number".into()),
        Value::Bool(_) => Value::String("bool".into()),
        Value::Null => Value::Null,
    }
}

/// Print notices as the gateway records them, with BankID links as QR codes.
async fn show_notices(log: Arc<Trajectory>) {
    let mut cursor = Seq::ZERO;
    loop {
        let latest = log.wait_after(cursor).await;
        for entry in log.read(cursor, latest).unwrap_or_default() {
            if let Event::Notice { text } = entry.event {
                eprintln!("\n{text}");
                if let Some(link) = connectors::find_bankid_link(&text)
                    && let Ok(code) = QrCode::new(link.as_bytes())
                {
                    eprintln!("{}", code.render::<Dense1x2>().quiet_zone(true).build());
                    eprintln!("Scan it with your phone's camera to open BankID.");
                }
            }
        }
        cursor = latest;
    }
}
