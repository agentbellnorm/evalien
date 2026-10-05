# evalien 👽

An autonomous agent whose only tool is `eval()`, driven purely by the Node.js event loop. No scheduler, no polling, no heartbeat. Just events.

Every observation (human input, runtime output, an eval settling) is appended to a log. One consumer follows the log, asks the model for code, and evaluates it. Evaluations run concurrently and their results are new events. That loop is the whole harness.

The agent isn't an assistant. It wakes up, explores, builds things, fetches data. A human can watch and type, but the agent drives itself.

## Components

- **Trajectory**: an append-only, validated event log: `start`, `stdin`, `stdout`, `stderr`, `eval`, `result`, `generation`. `waitAfter(seq)` is how consumers wake up, so no separate queue is needed.
- **DB**: the record store under the trajectory. It's SQLite (`node:sqlite`) in production and memory in tests. The agent can query its own history through `db` and create its own tables, which survive restarts.
- **Inference**: renders a cached window of the trajectory into a prompt and returns `{ code }` via structured output. Supports Anthropic, OpenAI, and Gemini, with pacing and a spend budget.
- **REPL**: evaluates the code as an async function body (`return` to inspect, `globalThis` to keep state) and captures stdout/stderr as it happens, including from callbacks.
- **Harness**: wires the components together. It decides which events wake the model and paces calls when no human is talking.

## How it fits together

```
  human ──stdin──┐
                 ▼
       ┌───────────────────┐   waitAfter(seq)    ┌──────────────────┐
       │    trajectory     │ ──────────────────▶ │    inference     │
       │  append-only log  │                     │ window → model   │
       │     (SQLite)      │ ◀── generation ──── │                  │
       └───────────────────┘                     └────────┬─────────┘
            ▲         ▲                                   │ { code }
            │         │ eval                              ▼
            │         └────────────────────────── ┌──────────────────┐
            │                                     │       REPL       │
            └────── result · stdout · stderr ──── │    eval(code)    │
                                                  └──────────────────┘
```

1. An event lands in the trajectory and wakes the consumer.
2. Inference snapshots the log up to a fixed `seq`, calls the model, and records a `generation`.
3. The code is recorded as `eval` and handed to the REPL. The consumer doesn't wait for it to finish.
4. Output and the eventual `result` are appended, which wakes step 1 again.

Only one model call runs at a time, and anything that arrives meanwhile is picked up on the next turn. An empty `code` reply means idle until the next event. The agent decides how long it waits through its own code:

```js
globalThis.checks = (globalThis.checks ?? 0) + 1;
await new Promise(resolve => setTimeout(resolve, 60_000));
return globalThis.checks;
```

## Running

Requires Node.js 24+.

```
cp .env.example .env   # set MODEL and its API key
npm install

npm run repl           # local
npm run repl:docker    # sandboxed container
npm run repl:reset     # wipe container state

npm test && npm run check
```

## Configuration

| Variable | Default | |
| --- | --- | --- |
| `MODEL` | `anthropic/claude-sonnet-5-5` | `anthropic/…`, `openai/…`, or `google/…` |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GOOGLE_GENERATIVE_AI_API_KEY` | | Only the selected provider's key is needed |
| `MODEL_BUDGET_USD` | `1` | Spend cap per process; the process exits with status 2 once it's reached |
| `MODEL_PRICING` | | `input,output[,cacheRead,cacheWrite]` in USD/M tokens, required for unpriced models |
| `MODEL_MAX_OUTPUT_TOKENS` | `4096` | |
| `MODEL_BASE_URL` | | Overrides the provider's API base URL |

## Reports

```
npm run report                       # summary and quality checks over ./agent.db
npm run report -- --timeline --last  # latest run, as the model saw it
```

The report fails when more than 5% of evals don't parse, when human input takes more than 60s to reach the model, or when the prompt cache hit rate is below 50%.

## Sandbox

The Docker image runs with a read-only root (writable `/tmp` and `/data`), `--cap-drop=ALL`, `no-new-privileges`, 512 MB of memory, 1 CPU, and bridge networking. Environment variables are cleared after the model config is read.

## Layout

Folders are modules. They share types through `src/contracts.mts` and never import each other, and `test/boundaries.test.mts` enforces this. `src/main.mts` and `src/report.mts` are the composition roots.

`db/` · `trajectory/` · `inference/` · `evaluation/` · `harness/` · `terminal/` · `quality/` · `lifecycle/`

## License

[MIT](LICENSE)
