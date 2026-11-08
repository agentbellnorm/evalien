//! Where the browser lands after signing in: `GET /oauth/callback?state&code`.
//! It's reachable only from the tailnet, and a code is only accepted for a
//! `state` the gateway issued, with a PKCE verifier that never left it.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use super::Connectors;

const MAX_REQUEST: usize = 16 << 10;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
pub const CALLBACK_PATH: &str = "/oauth/callback";

/// Answer callbacks on `listener` until the process ends.
pub async fn serve(listener: TcpListener, connectors: Arc<Connectors>, debug: fn(&str)) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(stream, connectors.clone()));
            }
            Err(error) => {
                debug(&format!("connectors: accepting a callback failed: {error}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn answer(mut stream: TcpStream, connectors: Arc<Connectors>) {
    let Ok(Some(head)) = tokio::time::timeout(READ_TIMEOUT, read_head(&mut stream)).await else {
        return;
    };
    let target = head
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|rest| rest.split(' ').next());
    let (status, message) =
        match target.and_then(|target| reqwest::Url::parse(&format!("http://callback{target}")).ok()) {
            Some(url) if url.path() == CALLBACK_PATH => {
                let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
                let state = query.get("state").map(String::as_str).unwrap_or_default();
                let result = match (query.get("code"), query.get("error")) {
                    (_, Some(error)) => connectors.refuse(state, error),
                    (Some(code), None) => connectors.complete(state, code).await,
                    (None, None) => Err("the provider sent neither a code nor an error".to_owned()),
                };
                match result {
                    Ok(name) => ("200 OK", format!("Signed in to {name}. You can close this tab.")),
                    Err(reason) => ("400 Bad Request", reason),
                }
            }
            _ => ("404 Not Found", "Not found.".to_owned()),
        };
    let body = page(&message);
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

fn page(message: &str) -> String {
    let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<!doctype html><meta name=viewport content=\"width=device-width\"><title>evalien</title>\
         <body style=\"font:16px system-ui;margin:3em auto;max-width:32em\"><p>{escaped}</p>"
    )
}

/// The request line and headers, or `None` if the client never sends them.
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buffer = [0; 2048];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 || head.len() + n > MAX_REQUEST {
            return None;
        }
        head.extend_from_slice(&buffer[..n]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}
