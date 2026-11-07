//! The JavaScript side of the sandbox: QuickJS compiled to
//! wasm32-unknown-unknown.
//!
//! The module's whole world is the `evalien` imports below plus a clock that
//! rquickjs needs. There is no WASI, so there are no files, sockets, env or
//! args to reach for. Everything the agent can do goes through `call`.
//!
//! The guest never blocks. Each export runs JS until the job queue is empty and
//! returns. The host owns the event loop: it decides when timers fire and when
//! host calls complete, then calls back in.
//!
//! Safety contract for every export taking `(ptr, len)`: the buffer came from
//! `alloc(len)` and ownership passes to the export, which frees it.
#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;

use rquickjs::{Context, Ctx, Function, Object, Persistent, Runtime};

#[link(wasm_import_module = "evalien")]
unsafe extern "C" {
    /// Start host capability `name` with JSON `args`. Completes via `complete(id, ..)`.
    fn call(id: u32, name_ptr: *const u8, name_len: usize, args_ptr: *const u8, args_len: usize);
    /// Fire `fire(id)` after `ms`.
    fn timer_set(id: u32, ms: f64);
    fn timer_clear(id: u32);
    /// Eval `id` finished. `ok` is 1 for a value, 0 for a thrown error.
    fn settled(id: u32, ok: u32, ptr: *const u8, len: usize);
    /// 1 = stdout, 2 = stderr.
    fn output(stream: u32, ptr: *const u8, len: usize);
    /// Polled by QuickJS every few thousand instructions. Nonzero stops the
    /// running script with an uncatchable error.
    fn interrupt() -> u32;
}

struct State {
    rt: Runtime,
    ctx: Context,
    bridge: Persistent<Object<'static>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const PRELUDE: &str = include_str!("prelude.js");

/// Buffer for the host to write into. Ownership passes to whichever export
/// receives the pointer.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

unsafe fn take(ptr: *mut u8, len: usize) -> String {
    let bytes = unsafe { Vec::from_raw_parts(ptr, len, len.max(1)) };
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Set up the runtime. `config` is JSON: `{ "capabilities": [names] }`.
/// Returns 0 on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init(memory_limit: u32, stack_limit: u32, config_ptr: *mut u8, config_len: usize) -> u32 {
    let config = unsafe { take(config_ptr, config_len) };
    match setup(memory_limit as usize, stack_limit as usize, &config) {
        Ok(state) => {
            STATE.with(|s| *s.borrow_mut() = Some(state));
            0
        }
        Err(message) => {
            emit(2, &format!("guest init failed: {message}"));
            1
        }
    }
}

fn setup(memory_limit: usize, stack_limit: usize, config: &str) -> Result<State, String> {
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    rt.set_memory_limit(memory_limit);
    rt.set_max_stack_size(stack_limit);
    rt.set_interrupt_handler(Some(Box::new(|| unsafe { interrupt() != 0 })));
    let ctx = Context::full(&rt).map_err(|e| e.to_string())?;

    let bridge = ctx.with(|ctx| -> rquickjs::Result<_> {
        let raw = Object::new(ctx.clone())?;
        raw.set(
            "call",
            Function::new(ctx.clone(), |id: u32, name: String, args: String| unsafe {
                call(id, name.as_ptr(), name.len(), args.as_ptr(), args.len())
            })?,
        )?;
        raw.set(
            "timerSet",
            Function::new(ctx.clone(), |id: u32, ms: f64| unsafe { timer_set(id, ms) })?,
        )?;
        raw.set(
            "timerClear",
            Function::new(ctx.clone(), |id: u32| unsafe { timer_clear(id) })?,
        )?;
        raw.set(
            "settled",
            Function::new(ctx.clone(), |id: u32, ok: bool, text: String| unsafe {
                settled(id, ok as u32, text.as_ptr(), text.len())
            })?,
        )?;
        raw.set(
            "output",
            Function::new(ctx.clone(), |stream: u32, text: String| emit(stream, &text))?,
        )?;

        let config = ctx.json_parse(config)?;
        let install: Function = ctx.eval(PRELUDE)?;
        let bridge: Object = install.call((raw, config))?;
        Ok(Persistent::save(&ctx, bridge))
    });
    let bridge = bridge.map_err(|e| ctx.with(|ctx| describe(&ctx, e)))?;
    Ok(State { rt, ctx, bridge })
}

/// Evaluate `code` as the body of an async function. Settles via `settled(id, ..)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eval(id: u32, ptr: *mut u8, len: usize) {
    let code = unsafe { take(ptr, len) };
    // An uncatchable error (interrupt) in the eval's first synchronous run
    // escapes without rejecting its promise. We know whose it was, so settle it.
    if let Some(message) = dispatch("evaluate", (id, code)) {
        unsafe { settled(id, 0, message.as_ptr(), message.len()) };
    }
}

/// Finish host call `id`. `payload` is JSON: the value if `ok`, else an error message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn complete(id: u32, ok: u32, ptr: *mut u8, len: usize) {
    let payload = unsafe { take(ptr, len) };
    dispatch("complete", (id, ok != 0, payload));
}

#[unsafe(no_mangle)]
pub extern "C" fn fire(id: u32) {
    dispatch("fire", (id,));
}

/// Call a bridge method, then drain the job queue so every promise that can
/// make progress does before control returns to the host. Returns the error if
/// the method itself threw.
fn dispatch<A>(method: &str, args: A) -> Option<String>
where
    A: for<'js> rquickjs::function::IntoArgs<'js>,
{
    STATE.with(|s| {
        let state = s.borrow();
        let Some(state) = state.as_ref() else {
            emit(2, "guest not initialized");
            return Some("guest not initialized".into());
        };
        let failure = state.ctx.with(|ctx| {
            let result = state
                .bridge
                .clone()
                .restore(&ctx)
                .and_then(|bridge| bridge.get::<_, Function>(method))
                .and_then(|f| f.call::<_, ()>(args));
            result.err().map(|e| describe(&ctx, e))
        });
        if let Some(message) = &failure {
            emit(2, message);
        }
        loop {
            match state.rt.execute_pending_job() {
                Ok(true) => continue,
                Ok(false) => break,
                // The prelude handles rejections itself; what reaches here is an
                // interrupt or an out-of-memory error, so the slice is over.
                Err(e) => {
                    let message = e.0.with(|ctx| describe(&ctx, rquickjs::Error::Exception));
                    emit(2, &message);
                }
            }
        }
        failure
    })
}

fn describe(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    if !matches!(error, rquickjs::Error::Exception) {
        return error.to_string();
    }
    let caught = ctx.catch();
    if let Some(e) = caught.as_exception() {
        let message = e.message().unwrap_or_default();
        return match e.stack() {
            Some(stack) if !stack.is_empty() => format!("{message}\n{stack}"),
            _ => message,
        };
    }
    ctx.json_stringify(caught)
        .ok()
        .flatten()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| "unknown exception".into())
}

fn emit(stream: u32, text: &str) {
    unsafe { output(stream, text.as_ptr(), text.len()) }
}
