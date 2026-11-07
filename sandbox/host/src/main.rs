//! Interactive REPL over the sandbox: each line is an eval, and events print as
//! they arrive. Evals run concurrently, so you can type while one sleeps.

use std::{sync::Arc, time::Instant};

use anyhow::Result;
use evalien_sandbox::{Event, Limits, Sandbox, capabilities::Demo};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let started = Instant::now();
    let mut sandbox = Sandbox::new(Arc::new(Demo), Limits::default())?;
    eprintln!(
        "sandbox ready in {:?}. capabilities: echo, delay. one eval per line.",
        started.elapsed()
    );

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            line = lines.next_line(), if stdin_open => match line? {
                Some(code) if code.trim().is_empty() => {}
                Some(code) => {
                    let id = sandbox.eval(&code)?;
                    eprintln!("[{id}] started");
                }
                None => stdin_open = false,
            },
            Some(event) = sandbox.next_event(), if !sandbox.is_idle() => print(event),
            else => break,
        }
    }
    let unsettled: Vec<_> = sandbox.pending().collect();
    if !unsettled.is_empty() {
        eprintln!("exiting with unsettled evals: {unsettled:?}");
    }
    Ok(())
}

fn print(event: Event) {
    match event {
        Event::Stdout(text) => println!("{text}"),
        Event::Stderr(text) => eprintln!("\x1b[33m{text}\x1b[0m"),
        Event::Settled { id, ok: true, value } => println!("[{id}] => {value}"),
        Event::Settled { id, ok: false, value } => println!("\x1b[31m[{id}] !! {value}\x1b[0m"),
        Event::Reset { reason } => eprintln!("\x1b[31mruntime reset: {reason}\x1b[0m"),
    }
}
