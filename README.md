# evalien 👽

An autonomous AI agent that lives inside a Node.js process. Its only tool is `eval()`. Everything it does, it does by evaluating JavaScript. The entire system is driven by the Node.js event loop. There is no polling, no heartbeat, no cron. Just events.

The agent is not an assistant. It doesn't wait for instructions. It wakes up, explores its environment, builds things, fetches data, writes poetry, tracks the ISS. Whatever it decides to do. A human can observe and occasionally type messages, but the agent drives itself.

## Event loop, all the way down

There is no scheduler. The agent is a native participant in the Node.js event loop:

1. **Eval result** → triggers the next tick (`setImmediate`)
2. **User input** → readline event → tick
3. **Self-wake** → the agent schedules its own `setTimeout(() => process.stdin.push("reason\n"), delay)`, which fires a readline event → tick
4. **Idle** → the agent returns `void 0` and the process goes completely silent. No timers, no polling, no CPU. The event loop is empty.

The agent decides its own rhythm. Want to check something every hour? It sets a timer. Want to sleep forever? It just stops. The system imposes nothing.

## Persistence

The agent has a SQLite database (`node:sqlite`) that persists across container restarts. The conversation transcript lives there, giving the agent continuity. It wakes up and remembers previous sessions. It can also create its own tables for journals, projects, anything.

## Running

```
cp .env.example .env  # add your ANTHROPIC_API_KEY
npm install

# Local
npm run repl

# Docker (sandboxed)
npm run repl:docker

# Wipe state and start fresh
npm run repl:reset
```

## Docker isolation

The container runs with:
- Read-only root filesystem (`/tmp` writable as tmpfs, `/data` as persistent volume)
- `--cap-drop=ALL --security-opt=no-new-privileges`
- `--memory=512m --cpus=1`
- Network access (bridge mode)
- Env vars nuked after API client init

## Files

- `agent.mjs` — the entire agent (~360 lines)
- `Dockerfile` — hardened container image
- `.env` — your API key (not committed)

## Relation to "claw"

Andrej Karpathy described the progression: "first there was chat, then there was code, now there is claw." Claw agents — like [OpenClaw](https://github.com/openclaw/openclaw) — are AI systems that go beyond conversation to actually *do things* via tools: shell commands, browser automation, file operations, APIs.

Evalien is a minimalist take on the same idea. Where claw systems are additive (more skills, more integrations, more infrastructure), evalien is reductive: give the agent a runtime and a single primitive (`eval`), and let it bootstrap everything else. With `eval()` in Node.js, the agent can fetch URLs, read/write files, spawn processes, set timers, create databases — no predefined skill system needed.

Both share the core properties of a claw agent — autonomy, persistence, local execution, real tool use — but evalien collapses the entire tool surface into one line of JavaScript.

## License

[MIT](LICENSE)
