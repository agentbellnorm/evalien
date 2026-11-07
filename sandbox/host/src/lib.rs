//! Runs untrusted agent JavaScript in QuickJS compiled to WebAssembly.
//!
//! The guest only has what [`Capabilities`] grants. It can't see the process,
//! files, env, the network or the API key, because none of those exist in its
//! world. Every call out goes through the host, which is where policy lives.
//!
//! The host owns the event loop, so this mirrors evalien. [`Sandbox::eval`]
//! starts an evaluation and returns immediately. Output, host call results,
//! timers and settled evals come out of [`Sandbox::next_event`] as they happen,
//! and evaluations run concurrently.
//!
//! Two CPU limits apply to every synchronous slice of guest execution:
//! - soft: QuickJS polls `interrupt` and throws an uncatchable error. Globals survive.
//! - hard: a wasmtime epoch deadline traps the instance. It's thrown away and
//!   replaced, pending evals fail, and globals are lost.

pub mod capabilities;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use serde_json::Value;
use tokio::{sync::mpsc, task::AbortHandle};
use wasmtime::{
    Caller, Engine, Extern, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, Trap, TypedFunc,
};

pub use capabilities::{BoxFuture, Capabilities};

include!("engine_config.rs");

/// The guest, precompiled for this target by build.rs.
const GUEST_CWASM: &[u8] = include_bytes!(env!("GUEST_CWASM"));
const JS_STACK: u32 = 128 << 10;
const EPOCH_TICK: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
pub struct Limits {
    /// Cap on the guest's linear memory. QuickJS gets most of it as its heap
    /// limit, so most out-of-memory errors are catchable JS errors instead of traps.
    pub memory_bytes: usize,
    /// Per slice: QuickJS throws an uncatchable error and the runtime survives.
    pub soft_slice: Duration,
    /// Per slice: the instance is killed and replaced.
    pub hard_slice: Duration,
    /// Output bytes per slice, across console calls and eval results.
    pub output_per_slice: usize,
    /// Largest single output chunk or result.
    pub max_chunk: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 << 20,
            soft_slice: Duration::from_secs(1),
            hard_slice: Duration::from_secs(3),
            output_per_slice: 1 << 20,
            max_chunk: 64 << 10,
        }
    }
}

pub type EvalId = u32;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Stdout(String),
    Stderr(String),
    Settled {
        id: EvalId,
        ok: bool,
        value: String,
    },
    /// The instance was killed. Pending evals settle as failed right after this.
    Reset {
        reason: String,
    },
}

pub struct Sandbox {
    engine: Engine,
    module: Module,
    linker: Linker<Guest>,
    capabilities: Arc<dyn Capabilities>,
    limits: Limits,
    live: Option<Live>,
    /// Bumped on every reset so completions from a dead instance are dropped.
    generation: u64,
    tx: mpsc::UnboundedSender<(u64, Completion)>,
    rx: mpsc::UnboundedReceiver<(u64, Completion)>,
    calls: HashMap<u32, AbortHandle>,
    timers: HashMap<u32, (u64, AbortHandle)>,
    next_token: u64,
    pending: HashSet<EvalId>,
    next_eval: EvalId,
    events: VecDeque<Event>,
    ticker: Arc<AtomicBool>,
}

struct Live {
    store: Store<Guest>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    eval: TypedFunc<(u32, u32, u32), ()>,
    complete: TypedFunc<(u32, u32, u32, u32), ()>,
    fire: TypedFunc<u32, ()>,
}

/// Per-instance state the imports write into. The host applies it after each slice.
struct Guest {
    limits: StoreLimits,
    deadline: Instant,
    interrupted: bool,
    output_left: usize,
    max_chunk: usize,
    ops: Vec<Op>,
}

enum Op {
    Call { id: u32, name: String, args: String },
    TimerSet { id: u32, ms: f64 },
    TimerClear { id: u32 },
    Settled { id: u32, ok: bool, value: String },
    Output { stream: u32, text: String },
}

enum Completion {
    Call { id: u32, result: Result<Value, String> },
    Timer { id: u32, token: u64 },
}

impl Sandbox {
    /// Must be called inside a tokio runtime: host calls and timers are tasks.
    pub fn new(capabilities: Arc<dyn Capabilities>, limits: Limits) -> Result<Self> {
        let engine = Engine::new(&engine_config())?;
        // SAFETY: the artifact was produced by build.rs with the same wasmtime
        // and config, and is embedded in this binary, so it's as trusted as
        // the binary itself. wasmtime still checks version and config match.
        let module =
            unsafe { Module::deserialize(&engine, GUEST_CWASM) }.map_err(|e| anyhow!("loading guest: {e:#}"))?;
        let linker = imports(&engine)?;

        let ticker = Arc::new(AtomicBool::new(true));
        {
            let (engine, running) = (engine.clone(), ticker.clone());
            thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            });
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let mut sandbox = Self {
            engine,
            module,
            linker,
            capabilities,
            limits,
            live: None,
            generation: 0,
            tx,
            rx,
            calls: HashMap::new(),
            timers: HashMap::new(),
            next_token: 0,
            pending: HashSet::new(),
            next_eval: 1,
            events: VecDeque::new(),
            ticker,
        };
        sandbox.live = Some(sandbox.instantiate()?);
        Ok(sandbox)
    }

    /// Start evaluating `code` as an async function body. Returns at the
    /// first `await` (or on completion), and the result arrives as [`Event::Settled`].
    pub fn eval(&mut self, code: &str) -> Result<EvalId> {
        if self.live.is_none() {
            self.live = Some(self.instantiate()?);
        }
        let id = self.next_eval;
        self.next_eval += 1;
        self.pending.insert(id);
        let code = code.to_owned();
        self.enter(move |live| {
            let (ptr, len) = live.write(code.as_bytes())?;
            live.eval.call(&mut live.store, (id, ptr, len))
        });
        Ok(id)
    }

    /// The next thing that happened. `None` means nothing can happen until the
    /// next `eval`: no queued events, timers or host calls. Cancel-safe.
    pub async fn next_event(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Some(event);
            }
            if self.is_idle() {
                return None;
            }
            let (generation, completion) = self.rx.recv().await?;
            if generation != self.generation {
                continue;
            }
            match completion {
                Completion::Call { id, result } => {
                    if self.calls.remove(&id).is_none() {
                        continue;
                    }
                    let (ok, payload) = match result {
                        Ok(value) => (1, value.to_string()),
                        Err(message) => (0, Value::String(message).to_string()),
                    };
                    self.enter(move |live| {
                        let (ptr, len) = live.write(payload.as_bytes())?;
                        live.complete.call(&mut live.store, (id, ok, ptr, len))
                    });
                }
                Completion::Timer { id, token } => {
                    if self.timers.get(&id).map(|(t, _)| *t) != Some(token) {
                        continue;
                    }
                    self.timers.remove(&id);
                    self.enter(move |live| live.fire.call(&mut live.store, id));
                }
            }
        }
    }

    pub fn is_idle(&self) -> bool {
        self.events.is_empty() && self.calls.is_empty() && self.timers.is_empty()
    }

    /// Evals that haven't settled yet.
    pub fn pending(&self) -> impl Iterator<Item = EvalId> + '_ {
        self.pending.iter().copied()
    }

    fn instantiate(&self) -> Result<Live> {
        let limits = &self.limits;
        let guest = Guest {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .instances(1)
                .tables(4)
                .memories(1)
                .build(),
            deadline: Instant::now() + limits.soft_slice,
            interrupted: false,
            output_left: limits.output_per_slice,
            max_chunk: limits.max_chunk,
            ops: Vec::new(),
        };
        let mut store = Store::new(&self.engine, guest);
        store.limiter(|g| &mut g.limits);
        store.epoch_deadline_trap();
        store.set_epoch_deadline(ticks(limits.hard_slice));

        let instance: Instance = self.linker.instantiate(&mut store, &self.module)?;
        let init: TypedFunc<(u32, u32, u32, u32), u32> = instance.get_typed_func(&mut store, "init")?;
        let mut live = Live {
            memory: instance
                .get_memory(&mut store, "memory")
                .context("guest exports no memory")?,
            alloc: instance.get_typed_func(&mut store, "alloc")?,
            eval: instance.get_typed_func(&mut store, "eval")?,
            complete: instance.get_typed_func(&mut store, "complete")?,
            fire: instance.get_typed_func(&mut store, "fire")?,
            store,
        };

        let config = serde_json::json!({ "capabilities": self.capabilities.names() }).to_string();
        // Leave QuickJS headroom below the wasm cap for the shim, stack and fragmentation.
        let heap = (limits.memory_bytes / 4 * 3) as u32;
        let (ptr, len) = live.write(config.as_bytes())?;
        let status = init.call(&mut live.store, (heap, JS_STACK, ptr, len))?;
        let ops = std::mem::take(&mut live.store.data_mut().ops);
        if status != 0 {
            let detail = ops
                .into_iter()
                .filter_map(|op| match op {
                    Op::Output { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(anyhow!("guest init failed: {detail}"));
        }
        Ok(live)
    }

    /// Run one synchronous slice of guest code under the CPU limits, then
    /// apply whatever it asked for.
    fn enter(&mut self, f: impl FnOnce(&mut Live) -> wasmtime::Result<()>) {
        let Some(live) = self.live.as_mut() else { return };
        {
            let guest = live.store.data_mut();
            guest.deadline = Instant::now() + self.limits.soft_slice;
            guest.interrupted = false;
            guest.output_left = self.limits.output_per_slice;
        }
        live.store.set_epoch_deadline(ticks(self.limits.hard_slice));
        let result = f(live);
        let ops = std::mem::take(&mut live.store.data_mut().ops);
        let interrupted = live.store.data().interrupted;
        self.apply(ops);
        if interrupted {
            self.events.push_back(Event::Stderr(format!(
                "interrupted: a synchronous slice ran longer than {:?}",
                self.limits.soft_slice
            )));
        }
        if let Err(error) = result {
            self.reset(describe(&error, &self.limits));
        }
    }

    fn apply(&mut self, ops: Vec<Op>) {
        for op in ops {
            match op {
                Op::Output { stream, text } => self.events.push_back(match stream {
                    1 => Event::Stdout(text),
                    _ => Event::Stderr(text),
                }),
                Op::Settled { id, ok, value } => {
                    // The guest is untrusted: ignore unknown or repeated ids.
                    if self.pending.remove(&id) {
                        self.events.push_back(Event::Settled { id, ok, value });
                    }
                }
                Op::Call { id, name, args } => {
                    let capabilities = self.capabilities.clone();
                    let (tx, generation) = (self.tx.clone(), self.generation);
                    let task = tokio::spawn(async move {
                        let result = match serde_json::from_str(&args) {
                            Ok(args) => capabilities.call(&name, args).await,
                            Err(e) => Err(format!("invalid arguments: {e}")),
                        };
                        let _ = tx.send((generation, Completion::Call { id, result }));
                    });
                    if let Some(old) = self.calls.insert(id, task.abort_handle()) {
                        old.abort();
                    }
                }
                Op::TimerSet { id, ms } => {
                    let token = self.next_token;
                    self.next_token += 1;
                    let (tx, generation) = (self.tx.clone(), self.generation);
                    let delay = Duration::from_secs_f64((ms / 1000.0).clamp(0.0, 1e9));
                    let task = tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = tx.send((generation, Completion::Timer { id, token }));
                    });
                    if let Some((_, old)) = self.timers.insert(id, (token, task.abort_handle())) {
                        old.abort();
                    }
                }
                Op::TimerClear { id } => {
                    if let Some((_, task)) = self.timers.remove(&id) {
                        task.abort();
                    }
                }
            }
        }
    }

    /// Throw the instance away. A fresh one is created on the next eval.
    fn reset(&mut self, reason: String) {
        self.live = None;
        self.generation += 1;
        for (_, task) in self.calls.drain() {
            task.abort();
        }
        for (_, (_, task)) in self.timers.drain() {
            task.abort();
        }
        self.events.push_back(Event::Reset { reason: reason.clone() });
        let mut pending: Vec<_> = self.pending.drain().collect();
        pending.sort_unstable();
        for id in pending {
            self.events.push_back(Event::Settled {
                id,
                ok: false,
                value: format!("runtime reset: {reason}"),
            });
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.ticker.store(false, Ordering::Relaxed);
        for (_, task) in self.calls.drain() {
            task.abort();
        }
        for (_, (_, task)) in self.timers.drain() {
            task.abort();
        }
    }
}

impl Live {
    /// Copy bytes into a guest-owned buffer. The export that receives it frees it.
    fn write(&mut self, bytes: &[u8]) -> wasmtime::Result<(u32, u32)> {
        let len = u32::try_from(bytes.len()).map_err(|_| wasmtime::format_err!("payload too large"))?;
        let ptr = self.alloc.call(&mut self.store, len)?;
        self.memory.write(&mut self.store, ptr as usize, bytes)?;
        Ok((ptr, len))
    }
}

fn ticks(d: Duration) -> u64 {
    (d.as_millis() / EPOCH_TICK.as_millis()).max(1) as u64 + 1
}

fn describe(error: &wasmtime::Error, limits: &Limits) -> String {
    match error.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => format!(
            "a synchronous slice ran longer than {:?} and was killed",
            limits.hard_slice
        ),
        Some(Trap::StackOverflow) => "stack overflow".into(),
        Some(trap) => format!("guest trapped: {trap}"),
        None => format!("guest failed: {error:#}"),
    }
}

/// Read a guest string, truncated to `max` bytes.
fn read(caller: &mut Caller<'_, Guest>, ptr: u32, len: u32, max: usize) -> Option<String> {
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return None;
    };
    let (start, len) = (ptr as usize, len as usize);
    let take = len.min(max);
    let bytes = memory.data(&caller).get(start..start.checked_add(take)?)?;
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if take < len {
        text.push_str(&format!("… [{} bytes truncated]", len - take));
    }
    Some(text)
}

fn imports(engine: &Engine) -> Result<Linker<Guest>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap(
        "evalien",
        "call",
        |mut caller: Caller<'_, Guest>, id: u32, name_ptr: u32, name_len: u32, args_ptr: u32, args_len: u32| {
            // Names and args aren't output, so they get their own size limits.
            let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                return;
            };
            let data = memory.data(&caller);
            let slice = |ptr: u32, len: u32, max: usize| {
                let (start, len) = (ptr as usize, len as usize);
                (len <= max)
                    .then(|| data.get(start..start + len))
                    .flatten()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
            };
            let (Some(name), Some(args)) = (slice(name_ptr, name_len, 256), slice(args_ptr, args_len, 8 << 20)) else {
                return;
            };
            caller.data_mut().ops.push(Op::Call { id, name, args });
        },
    )?;
    linker.func_wrap(
        "evalien",
        "timer_set",
        |mut caller: Caller<'_, Guest>, id: u32, ms: f64| {
            caller.data_mut().ops.push(Op::TimerSet { id, ms });
        },
    )?;
    linker.func_wrap("evalien", "timer_clear", |mut caller: Caller<'_, Guest>, id: u32| {
        caller.data_mut().ops.push(Op::TimerClear { id });
    })?;
    linker.func_wrap(
        "evalien",
        "settled",
        |mut caller: Caller<'_, Guest>, id: u32, ok: u32, ptr: u32, len: u32| {
            // Results are always delivered, even past the output budget, so an
            // eval never hangs on a chatty slice.
            let max = caller.data().max_chunk;
            let value = read(&mut caller, ptr, len, max).unwrap_or_else(|| "[unreadable result]".into());
            caller.data_mut().ops.push(Op::Settled { id, ok: ok != 0, value });
        },
    )?;
    linker.func_wrap(
        "evalien",
        "output",
        |mut caller: Caller<'_, Guest>, stream: u32, ptr: u32, len: u32| {
            let guest = caller.data();
            let (left, max) = (guest.output_left, guest.max_chunk);
            if left == 0 {
                return;
            }
            let Some(mut text) = read(&mut caller, ptr, len, max.min(left)) else {
                return;
            };
            let guest = caller.data_mut();
            guest.output_left = left.saturating_sub(len as usize);
            if guest.output_left == 0 {
                text.push_str("\n[output limit for this slice reached; further output dropped]");
            }
            guest.ops.push(Op::Output { stream, text });
        },
    )?;
    linker.func_wrap("evalien", "interrupt", |mut caller: Caller<'_, Guest>| -> u32 {
        let guest = caller.data_mut();
        if Instant::now() >= guest.deadline {
            guest.interrupted = true;
            1
        } else {
            0
        }
    })?;
    linker.func_wrap("env", "__rquickjs_host_now_us", |_: Caller<'_, Guest>| -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as f64)
            .unwrap_or(0.0)
    })?;
    Ok(linker)
}
