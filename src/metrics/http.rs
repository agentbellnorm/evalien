//! A minimal HTTP endpoint for Prometheus: `GET /metrics` and nothing else.

use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::domain::Timestamp;

use super::Metrics;

/// Largest request head to read; a scrape is a few hundred bytes.
const MAX_REQUEST: usize = 8 << 10;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Answer scrapes on `listener` until the process ends.
pub async fn serve(listener: TcpListener, metrics: Arc<Metrics>, debug: fn(&str)) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(stream, metrics.clone()));
            }
            Err(error) => {
                debug(&format!("metrics: accepting a connection failed: {error}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn answer(mut stream: TcpStream, metrics: Arc<Metrics>) {
    let Ok(Some(head)) = tokio::time::timeout(READ_TIMEOUT, read_head(&mut stream)).await else {
        return;
    };
    let request_line = head.lines().next().unwrap_or_default();
    let (status, content_type, body) = match request_line.split(' ').take(2).collect::<Vec<_>>()[..] {
        ["GET", "/metrics"] => ("200 OK", "text/plain; version=0.0.4", metrics.render(Timestamp::now())),
        ["GET", _] => ("404 Not Found", "text/plain", "not found\n".to_owned()),
        _ => ("405 Method Not Allowed", "text/plain", "only GET /metrics\n".to_owned()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

/// The request line and headers, or `None` if the client never sends them.
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buffer = [0; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 || head.len() + n > MAX_REQUEST {
            return None;
        }
        head.extend_from_slice(&buffer[..n]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}
