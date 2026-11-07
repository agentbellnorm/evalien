//! What the guest is allowed to do. Each name becomes an async global in the
//! sandbox taking one JSON argument. This is the single choke point where
//! policy, auditing and secrets-by-reference go.

use std::{future::Future, pin::Pin, time::Duration};

use serde_json::{Value, json};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub trait Capabilities: Send + Sync + 'static {
    fn names(&self) -> Vec<String>;
    fn call(&self, name: &str, args: Value) -> BoxFuture<Result<Value, String>>;
}

/// Grants nothing. The guest can compute, log and set timers.
pub struct Nothing;

impl Capabilities for Nothing {
    fn names(&self) -> Vec<String> {
        Vec::new()
    }
    fn call(&self, name: &str, _: Value) -> BoxFuture<Result<Value, String>> {
        let message = format!("no capability named {name:?}");
        Box::pin(async move { Err(message) })
    }
}

/// Toy capabilities for exercising the bridge: `echo(x)` returns `x` and
/// `delay({ ms, value })` returns `value` after `ms`, both resolved by the host.
pub struct Demo;

impl Capabilities for Demo {
    fn names(&self) -> Vec<String> {
        vec!["echo".into(), "delay".into()]
    }
    fn call(&self, name: &str, args: Value) -> BoxFuture<Result<Value, String>> {
        match name {
            "echo" => Box::pin(async move { Ok(args) }),
            "delay" => Box::pin(async move {
                let ms = args["ms"].as_u64().ok_or("delay: ms must be a non-negative integer")?;
                tokio::time::sleep(Duration::from_millis(ms.min(60_000))).await;
                Ok(args.get("value").cloned().unwrap_or(json!(null)))
            }),
            _ => Nothing.call(name, args),
        }
    }
}
