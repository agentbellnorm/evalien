//! The agent binary end to end, against a fake model server on localhost.

use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use evalien::db::{self, Access, SqliteStore};
use evalien::domain::{Event, Outcome, TRAJECTORY_TABLE, Trajectory};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// How the fake model answers.
#[derive(Clone, Copy)]
enum Behavior {
    /// The first call returns this code; later calls end the turn.
    Reply { code: &'static str, output_tokens: u64 },
    /// Never answers.
    Hang,
}

/// A fake Anthropic Messages endpoint. Returns its base URL and a call counter.
async fn fake_model(behavior: Behavior) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let call = counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(answer(stream, behavior, call));
        }
    });
    (base, calls)
}

async fn answer(mut stream: TcpStream, behavior: Behavior, call: usize) {
    // Read the request: headers, then a body of Content-Length bytes.
    let mut request = Vec::new();
    let mut buffer = [0; 8192];
    let body_start = loop {
        let n = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map_or(0, |value| value.trim().parse().unwrap());
    while request.len() < body_start + length {
        let n = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..n]);
    }

    let (code, output_tokens) = match behavior {
        Behavior::Hang => return tokio::time::sleep(Duration::from_secs(3600)).await,
        Behavior::Reply { code, output_tokens } if call == 0 => (code, output_tokens),
        Behavior::Reply { .. } => ("", 10),
    };
    let body = json!({
        "content": [{ "type": "text", "text": json!({ "code": code }).to_string() }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 100, "output_tokens": output_tokens },
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

fn start_agent(dir: &Path, base_url: &str, budget: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_evalien"))
        // An empty directory, so no .env file is read.
        .current_dir(dir)
        .env_clear()
        .env("AGENT_DB_PATH", dir.join("agent.db"))
        .env("MODEL", "anthropic/claude-sonnet-5-5")
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("MODEL_BASE_URL", base_url)
        .env("MODEL_BUDGET_USD", budget)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn events(path: &Path) -> Vec<Event> {
    let Ok(connection) = db::open(path, Access::ReadOnly) else {
        return Vec::new();
    };
    let Ok(store) = SqliteStore::new(connection, TRAJECTORY_TABLE) else {
        return Vec::new();
    };
    let Ok(log) = Trajectory::new(store) else {
        return Vec::new();
    };
    log.read_all()
        .map(|entries| entries.into_iter().map(|e| e.event).collect())
        .unwrap_or_default()
}

async fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
}

/// Wait for the agent to exit, off the async runtime so the fake model keeps answering.
async fn exit_code(child: Child) -> (Option<i32>, String) {
    let waiting = tokio::task::spawn_blocking(move || child.wait_with_output());
    let output = tokio::time::timeout(Duration::from_secs(20), waiting)
        .await
        .expect("the agent did not exit")
        .unwrap()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_full_turn_is_recorded_and_sigint_stops_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let code = "console.log('hello from the sandbox'); return 6 * 7";
    let (base, calls) = fake_model(Behavior::Reply {
        code,
        output_tokens: 10,
    })
    .await;
    let agent = start_agent(dir.path(), &base, "1");
    let path = dir.path().join("agent.db");

    // The result wakes the model again, which ends the turn.
    until("the second model call", || calls.load(Ordering::SeqCst) >= 2).await;
    until("the second generation", || {
        events(&path)
            .iter()
            .filter(|e| matches!(e, Event::Generation { .. }))
            .count()
            == 2
    })
    .await;
    signal(&agent, "INT");
    let (status, stderr) = exit_code(agent).await;
    assert_eq!(status, Some(130), "{stderr}");

    let recorded = events(&path);
    assert_eq!(recorded[0], Event::Start);
    assert!(matches!(&recorded[2], Event::Eval { code: c } if c == code));
    assert!(recorded.contains(&Event::Stdout {
        text: "hello from the sandbox".into()
    }));
    assert!(
        recorded
            .iter()
            .any(|e| matches!(e, Event::Result { outcome: Outcome::Return, text, .. } if text == "42"))
    );
}

#[tokio::test]
async fn sigterm_stops_the_process_during_generation_and_leaves_the_database_readable() {
    let dir = tempfile::tempdir().unwrap();
    let (base, calls) = fake_model(Behavior::Hang).await;
    let agent = start_agent(dir.path(), &base, "1");
    until("the model call", || calls.load(Ordering::SeqCst) >= 1).await;
    signal(&agent, "TERM");
    let (status, stderr) = exit_code(agent).await;
    assert_eq!(status, Some(143), "{stderr}");
    assert_eq!(events(&dir.path().join("agent.db")), [Event::Start]);
}

#[tokio::test]
async fn a_spent_budget_pauses_with_a_notice_and_a_restart_remembers_todays_spend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let notices = |path: &Path| {
        events(path)
            .iter()
            .filter(|e| matches!(e, Event::Notice { .. }))
            .count()
    };
    // 10,000 output tokens at $10 per million is $0.10, over the $0.01 budget.
    let (base, calls) = fake_model(Behavior::Reply {
        code: "return 1",
        output_tokens: 10_000,
    })
    .await;
    let agent = start_agent(dir.path(), &base, "0.01");
    until("the pause notice", || notices(&path) == 1).await;
    // The agent stays up, paused, instead of exiting into a restart loop.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    signal(&agent, "TERM");
    assert_eq!(exit_code(agent).await.0, Some(143));
    assert!(
        events(&path)
            .iter()
            .any(|e| matches!(e, Event::Result { text, .. } if text == "1"))
    );

    // A restarted pod knows today's spend and doesn't call the model.
    let restarted = start_agent(dir.path(), &base, "0.01");
    until("the second pause notice", || notices(&path) == 2).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    signal(&restarted, "TERM");
    assert_eq!(exit_code(restarted).await.0, Some(143));
}
