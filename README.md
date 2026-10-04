# evalien 👽

An autonomous AI agent that lives inside a Node.js process. Its only tool is `eval()`. Everything it does, it does by evaluating JavaScript. The entire system is driven by the Node.js event loop. There is no polling, no heartbeat, no cron. Just events.

The agent is not an assistant. It doesn't wait for instructions. It wakes up, explores its environment, builds things, fetches data, writes poetry, tracks the ISS. Whatever it decides to do. A human can observe and occasionally type messages, but the agent drives itself.

## Event loop, all the way down

There is no scheduler. The agent is a native participant in the Node.js event loop:

1. **Startup** → a `start` event begins inference
2. **Eval result** → every completion, including `undefined` and exceptions, continues inference
3. **User input** → a readline event appends `stdin`, which continues inference even while evaluations are running
4. **Runtime output** → stdout/stderr writes, including asynchronous callbacks, append observations; they continue inference when no evaluation is pending

All observations enter the trajectory immediately. One async consumer follows it by sequence number: wait for new entries, snapshot the trajectory, generate code, and start evaluating it. Evaluation doesn't block the consumer. Each result is appended whenever its evaluation settles, so several evaluations can be in flight and settle in any order. Output from a pending evaluation doesn't request inference by itself; it's seen with the next result or input. Code that settles without I/O reports its result before the consumer decides again. Only one model request runs at a time. Observations arriving during it are retained for the next request. Closing human stdin leaves the process's other work running.

Calls with no human input since the previous call are paced. The first three run back to back. After that they're at least 5 seconds, 30 seconds, 2 minutes, then 10 minutes apart. Human input ends a wait and resets the pacing. An agent waiting longer in its own code isn't delayed further. Without pacing, an agent that considers itself done keeps checking in every few seconds.

The model replies with a `{ "code": "..." }` object, constrained by a JSON schema where the provider supports it. Empty code ends the turn. Nothing is evaluated or recorded, and the next waking observation starts another turn: input, a result from a pending evaluation, or output while nothing is pending. Code that completes immediately asks for the next step right away, so idling is an empty reply, not a returned status. The source is evaluated as an async function body: use `return` to inspect a value and `globalThis` to retain bindings. Local declarations do not survive an evaluation. The agent controls waiting through its code, and a human can still reach it while that code waits. For example:

```js
globalThis.checks = (globalThis.checks ?? 0) + 1;
console.log({ checks: globalThis.checks });
await new Promise(resolve => setTimeout(resolve, 60_000));
```

## Persistence

The agent has a SQLite database (`node:sqlite`) that persists across container restarts. The `trajectory` table contains an ordered log of observable events:

| Event | Contents |
| --- | --- |
| `start` | A fresh runtime; earlier live state is gone |
| `eval` | Source accepted for evaluation |
| `stdin` | Human input |
| `stdout`, `stderr` | Captured runtime output |
| `result` | Evaluation ID, `return` or `throw`, and captured value/error text |
| `generation` | A model call: model, the seq its prompt ran through, latency, finish reason, tokens, and cost |

Each entry has a sequence number and an observation timestamp. A result references the sequence number of its eval. A model call is recorded right before the eval it produced, including calls that failed. The agent sees `generation` entries in its prompt, but they don't wake it. Even `undefined` completions are recorded. Output is captured as it happens, independently of evaluation boundaries.

The trajectory library validates a discriminated union, serializes events as versioned JSON, and renders entries independently. Appending entries preserves earlier rendered text. Results contain captured text, so loading history never reconstructs live JavaScript objects or reruns code.

The trajectory sits on a generic record store: SQLite in production, memory in tests. The store keeps opaque records in sequence and knows nothing about events. `trajectory.waitAfter(seq)` returns the latest sequence if newer entries already exist, or waits for `append()` on that trajectory instance. Each consumer owns its cursor; reads leave the log intact. This connects event producers to inference without polling or a separate event queue. Arrivals during generation or evaluation are already in the log when the consumer resumes.

The runtime renders each entry as a stable text block for generation. A request reads through a fixed sequence number. The recent context window advances in 200-event steps, keeping roughly 500–700 events; moving its beginning establishes a new cache prefix. Entry bodies over 8,000 characters render as head and tail with a pointer to the full event, and the window drops older entries if it would exceed 400,000 characters, so output floods can't inflate the cost of each call. Older events remain queryable with `db`:

```js
return db.prepare("SELECT seq, timestamp, event FROM trajectory ORDER BY seq DESC LIMIT 10").all();
```

Runtime capture observes JavaScript writes to `process.stdout` and `process.stderr`, including console methods and writes from callbacks. Host diagnostics bypass capture. Writes directly to OS file descriptors, including a child process inheriting those descriptors, bypass this capture; pipe child output through the parent's streams to observe it.

The database can also hold the agent's own tables. History and saved data survive a restart; bindings, timers, and connections do not.

## Running

```
# Requires Node.js 24 or newer.
cp .env.example .env  # select a model and fill in its API key
npm install

# Local
npm run repl

# Docker (sandboxed)
npm run repl:docker

# Wipe state and start fresh
npm run repl:reset

# Local tests (injected generation and mocked HTTP; no API key or paid calls)
npm test
npm run check
```

## Models

Set `MODEL=provider/model-id` in `.env`. Only the selected provider's API key is needed:

| Provider | Example `MODEL` | API key variable |
| --- | --- | --- |
| Anthropic | `anthropic/claude-sonnet-5-5` | `ANTHROPIC_API_KEY` |
| OpenAI | `openai/gpt-5.4` | `OPENAI_API_KEY` |
| Google Gemini | `google/gemini-2.5-flash` | `GOOGLE_GENERATIVE_AI_API_KEY` |

Use any model ID supported by the selected provider and your account. The default is `anthropic/claude-sonnet-5-5`. `MODEL_MAX_OUTPUT_TOKENS` sets the generation budget (default `4096`); `MODEL_BASE_URL` optionally overrides the provider's API base URL. OpenAI uses the Responses API. These are direct API-key integrations.

Provider/model selection happens once at startup. Credentials and configuration are captured before environment variables are cleared. Switching providers changes configuration only.

The runtime depends on an injected function with this interface (`src/contracts.mts`):

```ts
type Generate = (input: { instructions: string; blocks: readonly string[] })
  => Promise<{ code: string; model: string; latencyMs: number; finishReason: string; usage: Usage }>;
```

`src/inference/ai-sdk.mts` implements it using AI SDK Core and the direct OpenAI, Anthropic, and Google providers. SDK types, API settings, pricing, and cache state stay inside that adapter. It requests a `{ code }` object through the provider's native structured output (`output_config.format` for Anthropic, `json_schema` text format for OpenAI, `responseJsonSchema` for Gemini), so replies can't be markup or fenced prose. It returns completed source with the call's usage and cost. Truncated, unparseable, or otherwise unsuccessful generation throws a `GenerationError` that carries the usage, before anything is evaluated. Harness tests inject this function directly.

For Anthropic, the adapter sets cache breakpoints on the instructions, the previous unchanged prompt boundary, and the newest block. OpenAI and Gemini use their default implicit prefix caching on supported models. The stored trajectory contains no provider-specific cache settings.

## Cost and run reports

Every billed model response, including a rejected truncated one, is a `generation` event in the trajectory. `MODEL_BUDGET_USD` (default `1`) caps spend per process. Once it's reached, the runtime makes no further model calls and exits with status 2. The call that crosses the limit still completes. Spend is counted in memory, so evaluated code editing its trajectory doesn't reset it. Rates are built in for `anthropic/claude-sonnet-5-5` and `anthropic/claude-opus-5-5`. Other models need `MODEL_PRICING=input,output[,cacheRead,cacheWrite]` in USD per million tokens, and startup fails without it.

```
npm run report                      # ./agent.db: summary and quality checks
npm run report -- --timeline --last # the latest run, rendered as the model sees it
npm run report:docker -- --timeline # the podman volume (uses the last built image)
```

The report exits 1 when a check fails:
- **Model output parses as JavaScript.** At most 5% of evaluations may be source that fails to compile, such as tool-call markup or markdown fences. Exceptions thrown at runtime don't count.
- **Human input reaches the model promptly.** Every stdin line is in a prompt within 60 seconds.
- **Prompt cache is reused.** With at least 3 calls, at least 50% of input tokens are cache reads.

## Docker isolation

The container runs with:
- Read-only root filesystem (`/tmp` writable as tmpfs, `/data` as persistent volume)
- `--cap-drop=ALL --security-opt=no-new-privileges`
- `--memory=512m --cpus=1`
- Network access (bridge mode)
- Env vars nuked after model configuration and credential capture

## Architecture

Modules share the types in `src/contracts.mts` and never import each other. Folders are modules. Top-level files are composition: the entry points `src/main.mts` (the agent) and `src/report.mts` (the report), and what they share. `test/boundaries.test.mts` enforces this, and checks that SQLite, readline, and the AI SDK stay inside the module that adapts them.

- `src/contracts.mts` — events, usage, and the interfaces: `RecordStore`, `EventLog`, `Generate`, `Evaluator`, `InputSource`, `OutputSource`
- `src/db/` — opening the database, and record stores (SQLite, and memory for tests); contents are opaque strings
- `src/trajectory/` — the versioned event codec and the event log over any record store
- `src/inference/` — the AI SDK adapter, provider configuration, pricing, and the spend budget
- `src/evaluation/` — runs source as an async function body with the globals it's given
- `src/harness/` — the runtime loop, wake rules and pacing (`policy.mts`), and rendering and the prompt window (`context.mts`)
- `src/terminal/` — line input, stdout/stderr capture, and the display that follows the trajectory
- `src/quality/` — run summary and quality checks over trajectory entries
- `src/lifecycle/` — taking the environment, stop signals, exit codes, and cleanup
- `src/config.mts` — everything read from the environment, and the trajectory table name
- `src/system-prompt.mts` — the agent's system prompt, built from the values the runtime uses
- `src/main.mts` — the agent's composition root
- `src/report.mts` — the report's composition root
- `test/` — unit tests per module, in-memory harness tests, capture and signal tests in real processes
- `Dockerfile` — hardened container image
- `.env` — your API key (not committed)

## Relation to "claw"

Andrej Karpathy described the progression: "first there was chat, then there was code, now there is claw." Claw agents — like [OpenClaw](https://github.com/openclaw/openclaw) — are AI systems that go beyond conversation to actually *do things* via tools: shell commands, browser automation, file operations, APIs.

Evalien is a minimalist take on the same idea. Where claw systems are additive (more skills, more integrations, more infrastructure), evalien is reductive: give the agent a runtime and a single primitive (`eval`), and let it bootstrap everything else. With `eval()` in Node.js, the agent can fetch URLs, read/write files, spawn processes, set timers, create databases — no predefined skill system needed.

Both share the core properties of a claw agent — autonomy, persistence, local execution, real tool use — but evalien collapses the entire tool surface into one line of JavaScript.

## License

[MIT](LICENSE)
