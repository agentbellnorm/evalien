# evalien 👽

An autonomous agent whose only tool is `eval()`, driven purely by an event loop. No scheduler, no polling, no heartbeat. Just events.

Every observation (human input, runtime output, an eval settling) is appended to a log. One consumer follows the log, asks the model for code, and evaluates it. Evaluations run concurrently and their results are new events. That loop is the whole harness.

The agent isn't an assistant. It wakes up, explores, builds things, fetches data. A human can watch and type, but the agent drives itself.

The agent's JavaScript runs in a sandbox: QuickJS compiled to a WebAssembly component, inside wasmtime, inside one Rust binary. It can't see the process, files, environment or API key, because none of those exist in its world. Its only ways out are the capabilities the host grants, and the host checks every call.

## Components

- **Trajectory**: an append-only, validated event log: `start`, `stdin`, `stdout`, `stderr`, `eval`, `result`, `generation`. `wait_after(seq)` is how consumers wake up, so no separate queue is needed.
- **DB**: the record store under the trajectory. It's SQLite in production and memory in tests.
- **Inference**: renders a cached window of the trajectory into a prompt and gets `{ code }` back through structured output. Anthropic, OpenAI and Gemini over their HTTP APIs, with pacing and a spend budget.
- **Evaluation**: the sandbox. It evaluates code as an async function body (`return` to inspect, `globalThis` to keep state) and reports output, timers and results as they happen.
- **Capabilities**: what the sandbox can reach. `db` is SQL on the agent's database, where it can keep its own tables and read its history but never change the trajectory. `fetch` reaches the public internet only, with addresses checked at connection time.
- **Harness**: wires the components together. It decides which events wake the model and paces calls when no human is talking.

## How it fits together

```
  human ──stdin──┐
                 ▼
       ┌───────────────────┐   wait_after(seq)   ┌──────────────────┐
       │    trajectory     │ ──────────────────▶ │    inference     │
       │  append-only log  │                     │ window → model   │
       │     (SQLite)      │ ◀── generation ──── │                  │
       └───────────────────┘                     └────────┬─────────┘
            ▲         ▲                                   │ { code }
            │         │ eval                              ▼
            │         └────────────────────────── ┌──────────────────┐
            │                                     │     sandbox      │
            └────── result · stdout · stderr ──── │  QuickJS (wasm)  │
                                                  └──────────────────┘
```

1. An event lands in the trajectory and wakes the consumer.
2. Inference snapshots the log up to a fixed `seq`, calls the model, and records a `generation`.
3. The code is recorded as `eval` and handed to the sandbox. The consumer waits only until the code first pauses.
4. Output and the eventual `result` are appended, which wakes step 1 again.

Only one model call runs at a time, and anything that arrives meanwhile is picked up on the next turn. An empty `code` reply means idle until the next event. The agent decides how long it waits through its own code:

```js
globalThis.checks = (globalThis.checks ?? 0) + 1;
await new Promise(resolve => setTimeout(resolve, 60_000));
return globalThis.checks;
```

## The sandbox

The guest and host meet at one contract, [`wit/sandbox.wit`](wit/sandbox.wit). The guest never blocks: it asks the host to start a timer or a capability call, and the host calls back in when the answer is ready.

| Limit | Default | What happens |
| --- | --- | --- |
| Soft CPU, per synchronous run | 1s | the code is interrupted with an uncatchable error, and globals survive |
| Hard CPU, per synchronous run | 3s | the instance is replaced, pending evals fail, and globals are lost |
| Memory | 64 MiB | QuickJS gets 75% as its heap, so running out is usually a catchable error |
| Output | 1 MiB per run, 64 KiB per chunk | truncated with a notice; results always get through |

Runaway recursion also replaces the instance: QuickJS has no stack check when built for WASI, so the guest's stack overflows and traps without touching anything else. An interrupt that hits after an `await` is reported, but its eval stays pending.

WASI is linked only because the C library expects it, with an empty context: no directories, environment or arguments, stdio discarded, sockets denied.

## Running

Requires Rust (the version is pinned in `rust-toolchain.toml`) and wasi-sdk, which compiles QuickJS for the guest.

```
./scripts/setup.sh       # wasi-sdk into .tools/
cp .env.example .env     # set MODEL and its API key

AGENT_DB_PATH=./agent.db cargo run --release

cargo test
cargo clippy --all-targets
```

In a container:

```
podman build -t evalien .
podman run -it --env-file=.env --cap-drop=ALL --security-opt=no-new-privileges \
  --memory=512m --cpus=1 --read-only -v evalien-data:/data evalien
```

The image is distroless and runs as a non-root user.

## Configuration

| Variable | Default | |
| --- | --- | --- |
| `MODEL` | `anthropic/claude-sonnet-5-5` | `anthropic/…`, `openai/…`, or `google/…` |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GOOGLE_GENERATIVE_AI_API_KEY` | | Only the selected provider's key is needed |
| `MODEL_BUDGET_USD` | `1` | Spend cap per process; the process exits with status 2 once it's reached |
| `MODEL_PRICING` | | `input,output[,cacheRead,cacheWrite]` in USD/M tokens, required for unpriced models |
| `MODEL_MAX_OUTPUT_TOKENS` | `16000` | |
| `MODEL_BASE_URL` | | Overrides the provider's API base URL |
| `AGENT_DB_PATH` | `/data/agent.db` | |

Variables in `./.env` are read too; the process environment wins.

## Reports

```
cargo run --release --bin report -- agent.db                    # summary and quality checks
cargo run --release --bin report -- agent.db --timeline --last  # latest run, as the model saw it
```

The report fails when more than 5% of evals don't parse, when human input takes more than 60s to reach the model, or when the prompt cache hit rate is below 50%.

## Layout

Folders under `src/` are modules. They share types through `domain` and never import each other, and `tests/boundaries.rs` enforces this. `main.rs` and `bin/report.rs` are the composition roots. No module uses `unsafe`; the guest's FFI is generated from the WIT contract.

`domain/` · `db/` · `harness/` · `inference/` · `evaluation/` · `terminal/` · `quality/`

`guest/` is the JavaScript side of the sandbox, built for `wasm32-wasip2` by `build.rs`.

## License

[MIT](LICENSE)
