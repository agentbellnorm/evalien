# evalien 👽

An autonomous AI agent that lives inside a Node.js process. Its only tool is `eval()`. Everything it does, it does by evaluating JavaScript. The entire system is driven by the Node.js event loop. There is no polling, no heartbeat, no cron. Just events.

The agent is not an assistant. It doesn't wait for instructions. It wakes up, explores its environment, builds things, fetches data, writes poetry, tracks the ISS. Whatever it decides to do. A human can observe and occasionally type messages, but the agent drives itself.

## Event loop, all the way down

There is no scheduler. The agent is a native participant in the Node.js event loop:

1. **Startup** → a `start` event begins inference
2. **Eval result** → every completion, including `undefined` and exceptions, continues inference
3. **User input** → a readline event appends `stdin`
4. **Runtime output** → stdout/stderr writes, including asynchronous callbacks, append observations

All observations enter the trajectory immediately. One async consumer follows it by sequence number: wait for new entries, snapshot the trajectory, generate code, and evaluate it. Every event except submitted `eval` source triggers inference. Only one model request/evaluation runs at a time. Observations arriving during either are retained for the next request. Closing human stdin leaves the process's other work running.

The model emits JavaScript source directly. The current evaluator runs an async function body: use `return` to inspect a value and `globalThis` to retain bindings. Local declarations do not survive an evaluation. The agent controls waiting through its code; inference waits for evaluation to finish. For example:

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

Each entry has a database sequence number and an observation timestamp. A result references the sequence number of its eval. Even `undefined` completions are recorded. Output is captured as it happens, independently of evaluation boundaries.

The trajectory library validates a discriminated union, serializes events as versioned JSON, and renders entries independently. Appending entries preserves earlier rendered text. Results contain captured text, so loading history never reconstructs live JavaScript objects or reruns code.

`trajectory.waitAfter(seq)` returns the latest sequence if newer entries already exist, or waits for `append()` on that trajectory instance. Each consumer owns its cursor; reads leave the log intact. This connects event producers to inference without polling or a separate event queue. Arrivals during generation or evaluation are already in the log when the consumer resumes.

The runtime renders each entry as a stable text block for generation. A request reads through a fixed sequence number. The recent context window advances in 200-event steps, keeping roughly 500–700 events; moving its beginning establishes a new cache prefix. Older events remain queryable with `db`:

```js
return db.prepare("SELECT seq, timestamp, event FROM trajectory ORDER BY seq DESC LIMIT 10").all();
```

Runtime capture observes JavaScript writes to `process.stdout` and `process.stderr`, including console methods and writes from callbacks. Host diagnostics bypass capture. Writes directly to OS file descriptors, including a child process inheriting those descriptors, bypass this capture; pipe child output through the parent's streams to observe it.

The database can also hold the agent's own tables. History and saved data survive a restart; bindings, timers, and connections do not.

## Running

```
# Requires Node.js 22 or newer.
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
| Anthropic | `anthropic/claude-sonnet-4-6` | `ANTHROPIC_API_KEY` |
| OpenAI | `openai/gpt-5.4` | `OPENAI_API_KEY` |
| Google Gemini | `google/gemini-2.5-flash` | `GOOGLE_GENERATIVE_AI_API_KEY` |

Use any model ID supported by the selected provider and your account. The default is `anthropic/claude-sonnet-4-6`. `MODEL_MAX_OUTPUT_TOKENS` sets the generation budget (default `4096`); `MODEL_BASE_URL` optionally overrides the provider's API base URL. OpenAI uses the Responses API. These are direct API-key integrations.

Provider/model selection happens once at startup. Credentials and configuration are captured before environment variables are cleared. Switching providers changes configuration only.

The runtime depends on an injected function with this complete interface:

```ts
type Generate = (input: {
  instructions: string;
  blocks: readonly string[];
}) => Promise<string>;
```

`src/llm.mts` implements it using AI SDK Core and the direct OpenAI, Anthropic, and Google providers. SDK types, API settings, usage reporting, and cache state stay inside that adapter. It returns completed source; truncated or otherwise unsuccessful generation throws before evaluation. Runtime tests inject this function directly.

For Anthropic, the adapter sets cache breakpoints on the instructions, the previous unchanged prompt boundary, and the newest block. OpenAI and Gemini use their default implicit prefix caching on supported models. The stored trajectory contains no provider-specific cache settings.

## Docker isolation

The container runs with:
- Read-only root filesystem (`/tmp` writable as tmpfs, `/data` as persistent volume)
- `--cap-drop=ALL --security-opt=no-new-privileges`
- `--memory=512m --cpus=1`
- Network access (bridge mode)
- Env vars nuked after model configuration and credential capture

## Files

- `src/main.mts` — configuration and startup
- `src/runtime.mts` — event producers and the sequential trajectory consumer
- `src/generation.mts` — provider-independent generation contract
- `src/system-prompt.mts` — the agent's system prompt
- `src/trajectory.mts` — event types, validated JSON codecs, deterministic rendering
- `src/trajectory-store.mts` — SQLite append, bounded reads, and cursor subscriptions
- `src/output.mts` — stdout/stderr capture and separate host writers
- `src/llm.mts` — AI SDK adapter, provider configuration, and prompt caching
- `src/eval.mts` — JavaScript evaluation
- `src/util.mts` — error handling and terminal formatting
- `test/` — codec, persistence, rendering, stream capture, and runtime integration tests
- `Dockerfile` — hardened container image
- `.env` — your API key (not committed)

## Relation to "claw"

Andrej Karpathy described the progression: "first there was chat, then there was code, now there is claw." Claw agents — like [OpenClaw](https://github.com/openclaw/openclaw) — are AI systems that go beyond conversation to actually *do things* via tools: shell commands, browser automation, file operations, APIs.

Evalien is a minimalist take on the same idea. Where claw systems are additive (more skills, more integrations, more infrastructure), evalien is reductive: give the agent a runtime and a single primitive (`eval`), and let it bootstrap everything else. With `eval()` in Node.js, the agent can fetch URLs, read/write files, spawn processes, set timers, create databases — no predefined skill system needed.

Both share the core properties of a claw agent — autonomy, persistence, local execution, real tool use — but evalien collapses the entire tool surface into one line of JavaScript.

## License

[MIT](LICENSE)
