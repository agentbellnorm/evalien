//! Runs agent JavaScript in QuickJS compiled to a WebAssembly component.
//!
//! The guest only has what [`Capabilities`] grants. It can't see the process,
//! files, environment, network or API keys, because none of those exist in
//! its world. Every call out goes through the host.
//!
//! The host owns the event loop, the same way the harness does: an evaluation
//! starts and returns at its first pause; output, timers, host calls and
//! settled results come back as events. Every synchronous run of guest code
//! (a "slice") has two CPU limits:
//! - soft: QuickJS polls `should-interrupt` and stops the code with an
//!   uncatchable error. Globals survive.
//! - hard: a wasmtime epoch deadline traps the instance. It's replaced,
//!   pending evaluations fail, and globals are lost.
//!
//! Runaway recursion ends the same way as the hard limit: QuickJS has no stack
//! check when built for WASI, so the guest's stack overflows and traps. The
//! stack sits below the guest's data, so overflowing it can't corrupt anything.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tokio::{sync::mpsc, task::AbortHandle};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::domain::{Event, Outcome, Seq};

use super::capabilities::Capabilities;
use bindings::evalien::sandbox::host::{self, Channel};

mod bindings {
    wasmtime::component::bindgen!({ path: "wit", world: "sandbox", exports: { default: async } });
}

const GUEST_COMPONENT: &[u8] = include_bytes!(env!("GUEST_COMPONENT"));
const EPOCH_TICK: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
pub struct Limits {
    /// Cap on the guest's memory. QuickJS gets most of it as its heap limit,
    /// so running out is usually a catchable JavaScript error.
    pub memory_bytes: usize,
    /// Per slice: the running code is interrupted and the runtime survives.
    pub soft_slice: Duration,
    /// Per slice: the instance is killed and replaced.
    pub hard_slice: Duration,
    /// Output bytes per slice, across console calls.
    pub output_per_slice: usize,
    /// Largest single output chunk or result.
    pub max_chunk: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_bytes: 64 << 20,
            soft_slice: Duration::from_secs(1),
            hard_slice: Duration::from_secs(3),
            output_per_slice: 1 << 20,
            max_chunk: 64 << 10,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("sandbox: {0}")]
pub struct SandboxError(String);

impl SandboxError {
    fn from(error: impl std::fmt::Display) -> Self {
        SandboxError(error.to_string())
    }
}

/// The compiled guest and the engine that runs it. Compiling takes a moment,
/// so it happens once per process; instances are cheap.
pub struct SandboxHost {
    engine: Engine,
    component: Component,
    linker: Linker<GuestState>,
    ticking: Arc<AtomicBool>,
}

impl SandboxHost {
    pub fn new() -> Result<Arc<Self>, SandboxError> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(SandboxError::from)?;
        let component = Component::new(&engine, GUEST_COMPONENT).map_err(SandboxError::from)?;

        let mut linker = Linker::new(&engine);
        // WASI is linked only because the C library expects it. The context
        // is empty: no preopened directories, environment or arguments, stdio
        // discarded, and sockets denied. The guest doesn't import filesystem
        // or socket interfaces at all.
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(SandboxError::from)?;
        bindings::Sandbox::add_to_linker::<GuestState, HasSelf<GuestState>>(&mut linker, |state| state)
            .map_err(SandboxError::from)?;

        // Epoch deadlines are measured in ticks of this clock.
        let ticking = Arc::new(AtomicBool::new(true));
        let (clock, running) = (engine.clone(), ticking.clone());
        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                thread::sleep(EPOCH_TICK);
                clock.increment_epoch();
            }
        });

        Ok(Arc::new(SandboxHost {
            engine,
            component,
            linker,
            ticking,
        }))
    }

    async fn instantiate(&self, limits: &Limits) -> Result<Instance, SandboxError> {
        let state = GuestState {
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(limits.memory_bytes).build(),
            deadline: Instant::now() + limits.soft_slice,
            interrupted: false,
            output_left: limits.output_per_slice,
            max_chunk: limits.max_chunk,
            requests: Vec::new(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.epoch_deadline_trap();
        store.set_epoch_deadline(ticks(limits.hard_slice));
        let guest = bindings::Sandbox::instantiate_async(&mut store, &self.component, &self.linker)
            .await
            .map_err(SandboxError::from)?;
        Ok(Instance { store, guest })
    }

    /// An instance for checking whether source compiles, without running it.
    pub async fn checker(&self) -> Result<SourceChecker, SandboxError> {
        Ok(SourceChecker {
            instance: self.instantiate(&Limits::default()).await?,
        })
    }
}

impl Drop for SandboxHost {
    fn drop(&mut self) {
        self.ticking.store(false, Ordering::Relaxed);
    }
}

pub struct SourceChecker {
    instance: Instance,
}

impl SourceChecker {
    /// Whether `code` compiles as an async function body. Exceptions it would
    /// throw at runtime don't count.
    pub async fn parses(&mut self, code: &str) -> Result<bool, SandboxError> {
        let Instance { store, guest } = &mut self.instance;
        store.set_epoch_deadline(ticks(Limits::default().hard_slice));
        guest.call_parses(store, code).await.map_err(SandboxError::from)
    }
}

/// One live guest: its store and its exports.
struct Instance {
    store: Store<GuestState>,
    guest: bindings::Sandbox,
}

/// Per-instance state the imports write into. The host acts on it after each slice.
struct GuestState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    deadline: Instant,
    interrupted: bool,
    output_left: usize,
    max_chunk: usize,
    requests: Vec<Request>,
}

/// What a slice of guest code asked the host to do.
enum Request {
    Call { call: u32, name: String, args: String },
    SetTimer { timer: u32, delay: Duration },
    ClearTimer { timer: u32 },
    Settled { eval: Seq, outcome: Outcome, text: String },
    Output { channel: Channel, text: String },
}

impl WasiView for GuestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl host::Host for GuestState {
    fn call(&mut self, call: u32, name: String, args: String) {
        self.requests.push(Request::Call { call, name, args });
    }

    fn set_timer(&mut self, timer: u32, delay_ms: f64) {
        let delay = Duration::try_from_secs_f64(delay_ms / 1000.0).unwrap_or(Duration::ZERO);
        self.requests.push(Request::SetTimer { timer, delay });
    }

    fn clear_timer(&mut self, timer: u32) {
        self.requests.push(Request::ClearTimer { timer });
    }

    fn settled(&mut self, eval: u64, outcome: host::Outcome, text: String) {
        let outcome = match outcome {
            host::Outcome::Returned => Outcome::Return,
            host::Outcome::Threw => Outcome::Throw,
        };
        // Results always get through, even past the output budget, so an
        // evaluation never hangs on a chatty slice.
        let text = truncate(text, self.max_chunk);
        self.requests.push(Request::Settled {
            eval: Seq::new(eval),
            outcome,
            text,
        });
    }

    fn output(&mut self, channel: Channel, text: String) {
        if self.output_left == 0 {
            return;
        }
        let length = text.len();
        let mut text = truncate(text, self.max_chunk.min(self.output_left));
        self.output_left = self.output_left.saturating_sub(length);
        if self.output_left == 0 {
            text.push_str("\n[output limit for this slice reached; further output dropped]");
        }
        self.requests.push(Request::Output { channel, text });
    }

    fn should_interrupt(&mut self) -> bool {
        self.interrupted = self.interrupted || Instant::now() >= self.deadline;
        self.interrupted
    }
}

/// Something the sandbox is waiting for, ready to be delivered back.
pub struct Wakeup {
    generation: u64,
    kind: WakeupKind,
}

enum WakeupKind {
    Call { call: u32, answer: Result<String, String> },
    Timer { timer: u32, token: u64 },
}

/// A guest with persistent globals, running evaluations concurrently.
pub struct Sandbox {
    host: Arc<SandboxHost>,
    capabilities: Capabilities,
    limits: Limits,
    /// `None` after the instance was killed, until the next evaluation.
    instance: Option<Instance>,
    /// Bumped on every reset, so wakeups for a dead instance are dropped.
    generation: u64,
    wakeups: mpsc::UnboundedSender<Wakeup>,
    arrivals: mpsc::UnboundedReceiver<Wakeup>,
    calls: HashMap<u32, AbortHandle>,
    timers: HashMap<u32, (u64, AbortHandle)>,
    next_token: u64,
    pending: HashSet<Seq>,
    events: Vec<Event>,
}

impl Sandbox {
    pub async fn new(host: Arc<SandboxHost>, capabilities: Capabilities, limits: Limits) -> Result<Self, SandboxError> {
        let (wakeups, arrivals) = mpsc::unbounded_channel();
        let mut sandbox = Sandbox {
            host,
            capabilities,
            limits,
            instance: None,
            generation: 0,
            wakeups,
            arrivals,
            calls: HashMap::new(),
            timers: HashMap::new(),
            next_token: 0,
            pending: HashSet::new(),
            events: Vec::new(),
        };
        sandbox.instance = Some(start_instance(&sandbox.host, &sandbox.capabilities, &sandbox.limits).await?);
        Ok(sandbox)
    }

    /// Start evaluating `code` as an async function body. Returns once it has
    /// run up to its first pause; its result arrives as an event.
    pub async fn evaluate(&mut self, eval: Seq, code: &str) {
        self.pending.insert(eval);
        if self.instance.is_none() {
            match start_instance(&self.host, &self.capabilities, &self.limits).await {
                Ok(instance) => self.instance = Some(instance),
                Err(error) => {
                    self.reset(&error.to_string());
                    return;
                }
            }
        }
        self.slice(async |instance: &mut Instance| {
            instance
                .guest
                .call_evaluate(&mut instance.store, eval.get(), code)
                .await
        })
        .await;
    }

    /// Whether a timer or host call is outstanding, so a wakeup will come.
    pub fn is_waiting(&self) -> bool {
        !self.calls.is_empty() || !self.timers.is_empty()
    }

    /// The next timer or host call that finished. Cancel-safe.
    pub async fn next_wakeup(&mut self) -> Option<Wakeup> {
        self.arrivals.recv().await
    }

    /// Hand a finished timer or host call back to the guest.
    pub async fn wake(&mut self, wakeup: Wakeup) {
        if wakeup.generation != self.generation {
            return;
        }
        match wakeup.kind {
            WakeupKind::Call { call, answer } => {
                if self.calls.remove(&call).is_none() {
                    return;
                }
                self.slice(async |instance: &mut Instance| {
                    let answer = answer.as_ref().map(String::as_str).map_err(String::as_str);
                    instance.guest.call_complete(&mut instance.store, call, answer).await
                })
                .await;
            }
            WakeupKind::Timer { timer, token } => {
                if self.timers.get(&timer).map(|(t, _)| *t) != Some(token) {
                    return;
                }
                self.timers.remove(&timer);
                self.slice(async |instance: &mut Instance| instance.guest.call_fire(&mut instance.store, timer).await)
                    .await;
            }
        }
    }

    /// Observations since the last call, in order.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Run one synchronous slice of guest code under the limits, then act on
    /// what it asked for.
    async fn slice(&mut self, run: impl AsyncFnOnce(&mut Instance) -> wasmtime::Result<()>) {
        let Some(instance) = self.instance.as_mut() else { return };
        let state = instance.store.data_mut();
        state.deadline = Instant::now() + self.limits.soft_slice;
        state.interrupted = false;
        state.output_left = self.limits.output_per_slice;
        instance.store.set_epoch_deadline(ticks(self.limits.hard_slice));

        let result = run(instance).await;
        let state = instance.store.data_mut();
        let requests = std::mem::take(&mut state.requests);
        let interrupted = state.interrupted;
        self.handle(requests);
        if interrupted {
            let soft = self.limits.soft_slice;
            self.events.push(Event::Stderr {
                text: format!("interrupted: a synchronous run took longer than {soft:?}"),
            });
        }
        if let Err(error) = result {
            self.reset(&describe(&error, &self.limits));
        }
    }

    fn handle(&mut self, requests: Vec<Request>) {
        for request in requests {
            match request {
                Request::Output {
                    channel: Channel::Stdout,
                    text,
                } => self.events.push(Event::Stdout { text }),
                Request::Output {
                    channel: Channel::Stderr,
                    text,
                } => self.events.push(Event::Stderr { text }),
                Request::Settled { eval, outcome, text } => {
                    // The guest is untrusted: ignore unknown or repeated IDs.
                    if self.pending.remove(&eval) {
                        self.events.push(Event::Result { eval, outcome, text });
                    }
                }
                Request::Call { call, name, args } => {
                    let capabilities = self.capabilities.clone();
                    let (wakeups, generation) = (self.wakeups.clone(), self.generation);
                    let task = tokio::spawn(async move {
                        let answer = capabilities.call(&name, &args).await;
                        let _ = wakeups.send(Wakeup {
                            generation,
                            kind: WakeupKind::Call { call, answer },
                        });
                    });
                    if let Some(previous) = self.calls.insert(call, task.abort_handle()) {
                        previous.abort();
                    }
                }
                Request::SetTimer { timer, delay } => {
                    let token = self.next_token;
                    self.next_token += 1;
                    let (wakeups, generation) = (self.wakeups.clone(), self.generation);
                    let task = tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = wakeups.send(Wakeup {
                            generation,
                            kind: WakeupKind::Timer { timer, token },
                        });
                    });
                    if let Some((_, previous)) = self.timers.insert(timer, (token, task.abort_handle())) {
                        previous.abort();
                    }
                }
                Request::ClearTimer { timer } => {
                    if let Some((_, task)) = self.timers.remove(&timer) {
                        task.abort();
                    }
                }
            }
        }
    }

    /// Throw the instance away. Pending evaluations fail, and a fresh
    /// instance starts with the next evaluation.
    fn reset(&mut self, reason: &str) {
        self.instance = None;
        self.generation += 1;
        self.cancel_everything();
        self.events.push(Event::Stderr {
            text: format!("runtime reset: {reason}. Globals, timers and pending evaluations are gone."),
        });
        let mut pending: Vec<Seq> = self.pending.drain().collect();
        pending.sort();
        for eval in pending {
            self.events.push(Event::Result {
                eval,
                outcome: Outcome::Throw,
                text: format!("runtime reset: {reason}"),
            });
        }
    }

    fn cancel_everything(&mut self) {
        for (_, task) in self.calls.drain() {
            task.abort();
        }
        for (_, (_, task)) in self.timers.drain() {
            task.abort();
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.cancel_everything();
    }
}

/// A new instance, initialized with the granted capabilities. Takes the parts
/// it needs rather than the sandbox, which isn't shareable across threads.
async fn start_instance(
    host: &SandboxHost,
    capabilities: &Capabilities,
    limits: &Limits,
) -> Result<Instance, SandboxError> {
    let mut instance = host.instantiate(limits).await?;
    // QuickJS gets most of the memory, leaving room for the stack and fragmentation.
    let config = bindings::Config {
        memory_limit: u32::try_from(limits.memory_bytes / 4 * 3).unwrap_or(u32::MAX),
        capabilities: capabilities.names(),
    };
    let Instance { store, guest } = &mut instance;
    guest
        .call_init(store, &config)
        .await
        .map_err(SandboxError::from)?
        .map_err(SandboxError)?;
    Ok(instance)
}

fn ticks(duration: Duration) -> u64 {
    let ticks = duration.as_millis() / EPOCH_TICK.as_millis();
    u64::try_from(ticks).unwrap_or(u64::MAX).max(1) + 1
}

fn describe(error: &wasmtime::Error, limits: &Limits) -> String {
    match error.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => format!(
            "a synchronous run took longer than {:?} and was killed",
            limits.hard_slice
        ),
        Some(Trap::StackOverflow) => "stack overflow".into(),
        Some(trap) => format!("the guest trapped: {trap}"),
        None => format!("the guest failed: {error:#}"),
    }
}

/// Cut text to at most `max` bytes on a character boundary, noting what was dropped.
fn truncate(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let dropped = text.len() - max;
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!("… [{dropped} bytes truncated]"));
    text
}
