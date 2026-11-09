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
- **Harness**: wires the components together. It decides which events wake the model and paces calls when no human is talking. When the day's budget is spent, it records a notice and pauses until midnight UTC.
- **Connectors**: a credential gateway. Requests the agent sends to a connected service's origins get its credentials attached on the way out; the agent never sees them. API keys, OAuth (the agent calls `connections.connect`, the human gets a sign-in link, tokens refresh by themselves), and Mobile BankID (an autostart link for the BankID app; the bank's session cookies are kept and kept alive while in use), starting with SEB ([design](docs/connectors.md)).
- **Chat**: how the human reaches the agent remotely. Their messages become `stdin`; the code the agent runs (as a highlighted block), its `console.log` output and the runtime's notices go back. Matrix is the first implementation of the `Chat` port; without one, the terminal is the channel.

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
| `MODEL_BUDGET_USD` | `1` | Spend cap per UTC day, counted from the recorded calls so restarts don't reset it. Once it's spent, model calls pause until midnight |
| `MODEL_PRICING` | | `input,output[,cacheRead,cacheWrite]` in USD/M tokens, required for unpriced models |
| `MODEL_MAX_OUTPUT_TOKENS` | `16000` | |
| `MODEL_BASE_URL` | | Overrides the provider's API base URL |
| `AGENT_DB_PATH` | `/data/agent.db` | |
| `MATRIX_HOMESERVER` | | Enables Matrix; the terminal is the channel otherwise |
| `MATRIX_ACCESS_TOKEN` | | The agent's own Matrix account |
| `MATRIX_ROOM_ID` | | The room to talk in. It must be unencrypted |
| `MATRIX_OWNER` | | The only user whose messages reach the agent, e.g. `@you:example.org` |
| `METRICS_ADDR` | | Serve Prometheus metrics at `http://<addr>/metrics`, e.g. `0.0.0.0:9464` |
| `CONNECTIONS_FILE` | | A TOML file of external services whose requests get credentials; see [docs/connectors.md](docs/connectors.md) |
| `VAULT_KEY` | | 32 bytes, base64: encrypts the tokens OAuth logins earn. Required with OAuth connections |
| `VAULT_PATH` | next to the database | Where the vault lives |
| `CALLBACK_ADDR`, `OAUTH_REDIRECT_URL` | | Where the OAuth callback listens, and the URL the browser is sent to. Required with OAuth connections |

Variables in `./.env` are read too; the process environment wins.

## Deploying to Kubernetes

The manifests in [`deploy/k8s/`](deploy/k8s/) run one pod with a Longhorn volume for `/data`, talking over the in-cluster Synapse. They hold no secrets.

1. Give the agent its own Matrix account:
   ```
   kubectl -n skum-matrix exec -it deploy/matrix-synapse -- \
     register_new_matrix_user http://localhost:8008 -c /theconfig/homeserver.yaml -u evalien --no-admin
   ```
2. From your own account, create a room with encryption off (it can't be turned off later) and invite the agent's account. Put the room ID exactly as Room settings → Advanced shows it, and your own user ID, in `deploy/k8s/03-config.yaml`.
3. Create the namespace and the `evalien-secrets` Secret. The script logs in as the agent to get its access token, and takes the Anthropic key from `.env` or asks for it:
   ```
   ./scripts/create-secrets.sh
   ```
4. Set the secrets the connections in `03-config.yaml` name, each prompted for so it's never echoed, and generate the vault key once:
   ```
   ./scripts/set-secret.sh GOOGLE_CLIENT_ID
   ./scripts/set-secret.sh GOOGLE_CLIENT_SECRET
   ./scripts/set-secret.sh VAULT_KEY --generate
   ```
   OAuth sign-ins land on `https://evalien.bushnet.diy/oauth/callback`, so the browser needs to be on the tailnet while signing in. Register that URL as the OAuth client's redirect URI.
5. Build and deploy through the cluster's pipeline: a Jenkins multibranch job on this repo builds the image (`Jenkinsfile`), pushes it to the cluster registry, and bumps the tag in `deploy/k8s/04-deployment.yaml`. ArgoCD deploys `deploy/k8s` from `main`; apply its Application once:
   ```
   kubectl apply -f deploy/argocd-application.yaml
   ```
6. Follow along with `kubectl -n evalien logs -f deploy/evalien`. The log is the trajectory as it grows.

The pod runs as non-root with a read-only root filesystem, no capabilities, no service account token, and a NetworkPolicy that allows only DNS, Synapse, and public HTTP(S).

## Metrics

With `METRICS_ADDR` set, the agent serves Prometheus metrics, read off the trajectory like everything else:

| Metric | Labels | |
| --- | --- | --- |
| `evalien_model_calls_total` | provider, model, finish_reason | Model calls, by how they ended |
| `evalien_model_tokens_total` | provider, model, class | Tokens: `input_uncached`, `input_cache_read`, `input_cache_write`, `output`, `output_reasoning` |
| `evalien_model_cost_usd_total` | provider, model | Spend in USD |
| `evalien_model_call_duration_seconds` | provider, model | Histogram of call latency |
| `evalien_budget_daily_usd`, `evalien_budget_spent_today_usd` | | The budget, and today's spend (UTC) |
| `evalien_events_total` | type | Trajectory events |
| `evalien_evaluation_duration_seconds` | outcome | Histogram from eval to result |
| `evalien_evaluations_pending` | | Evaluations still running |

The cache hit rate is `input_cache_read` over all `input_*` tokens. In the cluster, Prometheus finds the pod through its `prometheus.io/*` annotations. Import the dashboard with the monitoring repo's `grafana-sync.sh import deploy/grafana/evalien-dashboard.json`.

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
