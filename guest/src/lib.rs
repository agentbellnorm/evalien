//! The JavaScript side of the sandbox: QuickJS as a WebAssembly component.
//!
//! The component's contract is `wit/sandbox.wit`. Everything the agent can do
//! goes through the `host` imports there. The host links WASI with an empty
//! context only because the C library expects a clock and random numbers;
//! there are no files, sockets, environment or arguments to reach.
//!
//! The guest never blocks. Each export runs JavaScript until the job queue is
//! empty and returns. The host owns the event loop: it decides when timers
//! fire and when host calls complete, then calls back in.

#![deny(unsafe_code)]

use std::cell::RefCell;

use rquickjs::{Context, Ctx, Function, Object, Persistent, Runtime, function::IntoArgs};

use bindings::evalien::sandbox::host::{self, Channel, Outcome};
use bindings::{Config, Guest};

/// Generated from the WIT contract. The generated glue is the only unsafe code.
#[allow(unsafe_code)]
mod bindings {
    use super::Sandbox;
    wit_bindgen::generate!({ path: "../wit", world: "sandbox" });
    export!(Sandbox);
}

const PRELUDE: &str = include_str!("prelude.js");

struct Sandbox;

/// The live runtime, plus the bridge object the prelude returned.
struct State {
    runtime: Runtime,
    context: Context,
    bridge: Persistent<Object<'static>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

impl Guest for Sandbox {
    fn init(config: Config) -> Result<(), String> {
        let state = start(&config)?;
        STATE.with(|s| *s.borrow_mut() = Some(state));
        Ok(())
    }

    fn evaluate(eval: u64, code: String) {
        // An interrupt in the eval's first synchronous run escapes without
        // rejecting its promise. We know whose it was, so settle it here.
        if let Err(message) = dispatch("evaluate", (eval, code)) {
            host::settled(eval, Outcome::Threw, &message);
        }
    }

    fn complete(call: u32, answer: Result<String, String>) {
        let (ok, payload) = match answer {
            Ok(json) => (true, json),
            Err(message) => (false, message),
        };
        report(dispatch("complete", (call, ok, payload)));
    }

    fn fire(timer: u32) {
        report(dispatch("fire", (timer,)));
    }

    fn parses(code: String) -> bool {
        // Called on a fresh component, so it needs no prior init.
        let Ok(runtime) = Runtime::new() else { return false };
        let Ok(context) = Context::full(&runtime) else {
            return false;
        };
        // Evaluating the function expression compiles it without calling it.
        context.with(|ctx| ctx.eval::<(), _>(format!("(async () => {{{code}\n}})")).is_ok())
    }
}

fn start(config: &Config) -> Result<State, String> {
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(config.memory_limit as usize);
    runtime.set_interrupt_handler(Some(Box::new(host::should_interrupt)));
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;

    let bridge = context.with(|ctx| install(&ctx, &config.capabilities).map_err(|e| describe(&ctx, e)))?;
    Ok(State {
        runtime,
        context,
        bridge,
    })
}

/// Run the prelude with the raw host functions. It installs the agent-facing
/// globals and returns the bridge object the exports call into.
fn install<'js>(ctx: &Ctx<'js>, capabilities: &[String]) -> rquickjs::Result<Persistent<Object<'static>>> {
    let raw = Object::new(ctx.clone())?;
    raw.set(
        "call",
        Function::new(ctx.clone(), |call: u32, name: String, args: String| {
            host::call(call, &name, &args)
        })?,
    )?;
    raw.set(
        "setTimer",
        Function::new(ctx.clone(), |timer: u32, ms: f64| host::set_timer(timer, ms))?,
    )?;
    raw.set("clearTimer", Function::new(ctx.clone(), host::clear_timer)?)?;
    raw.set(
        "settled",
        Function::new(ctx.clone(), |eval: u64, returned: bool, text: String| {
            let outcome = if returned { Outcome::Returned } else { Outcome::Threw };
            host::settled(eval, outcome, &text);
        })?,
    )?;
    raw.set(
        "output",
        Function::new(ctx.clone(), |stderr: bool, text: String| {
            host::output(if stderr { Channel::Stderr } else { Channel::Stdout }, &text);
        })?,
    )?;

    let prelude: Function = ctx.eval(PRELUDE)?;
    let bridge: Object = prelude.call((raw, capabilities.to_vec()))?;
    Ok(Persistent::save(ctx, bridge))
}

/// Call a bridge method, then drain the job queue so every promise that can
/// make progress does before control returns to the host. Errs if the method
/// itself threw; errors from jobs are reported as output.
fn dispatch<A>(method: &str, args: A) -> Result<(), String>
where
    A: for<'js> IntoArgs<'js>,
{
    STATE.with(|s| {
        let state = s.borrow();
        let state = state.as_ref().ok_or("the sandbox is not initialized")?;
        let called = state.context.with(|ctx| {
            state
                .bridge
                .clone()
                .restore(&ctx)
                .and_then(|bridge| bridge.get::<_, Function>(method))
                .and_then(|function| function.call::<_, ()>(args))
                .map_err(|e| describe(&ctx, e))
        });
        // The prelude catches what agent code throws, so a failing job is an
        // interrupt or running out of memory.
        loop {
            match state.runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(job) => {
                    let message = job.0.with(|ctx| describe(&ctx, rquickjs::Error::Exception));
                    host::output(Channel::Stderr, &message);
                }
            }
        }
        called
    })
}

fn report(result: Result<(), String>) {
    if let Err(message) = result {
        host::output(Channel::Stderr, &message);
    }
}

fn describe(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    if !matches!(error, rquickjs::Error::Exception) {
        return error.to_string();
    }
    let caught = ctx.catch();
    if let Some(exception) = caught.as_exception() {
        let message = exception.message().unwrap_or_default();
        return match exception.stack() {
            Some(stack) if !stack.is_empty() => format!("{message}\n{stack}"),
            _ => message,
        };
    }
    ctx.json_stringify(caught)
        .ok()
        .flatten()
        .and_then(|text| text.to_string().ok())
        .unwrap_or_else(|| "unknown exception".into())
}
