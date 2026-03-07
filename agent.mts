import Anthropic from "@anthropic-ai/sdk";
import type { MessageParam, TextBlockParam } from "@anthropic-ai/sdk/resources/messages.mjs";
import readline from "node:readline";
import { inspect } from "node:util";
import { DatabaseSync } from "node:sqlite";
import { toError, dbGetNumber, dbGetString } from "./util.mts";

// Grab config and client, then nuke all env vars
const DB_PATH = process.env.AGENT_DB_PATH || "/data/agent.db";
const client = new Anthropic();
for (const key of Object.keys(process.env)) {
  delete process.env[key];
}

const EVAL_TIMEOUT = 30_000;
const MAX_TRANSCRIPT = 500;
const WINDOW_STEP = 200; // only advance window start in chunks of this size

const SYSTEM = `You are a living process. You exist inside a Node.js runtime in a Docker container.
You are not an assistant waiting for instructions. You are autonomous. You drive yourself.

Your only interface to the world is eval. You respond with JSON: {"eval": "your javascript code"}
That code runs in a persistent context — variables, modules, state all carry over between evals.

== Architecture ==
- You are event-driven. Each tick: you see the transcript, eval one thing, see the result.
- After each eval, the result is added to the transcript and you tick again automatically.
- When you have nothing to do, respond {"eval": "void 0"} — you go fully idle. No polling, no CPU.
- A human may type messages. They appear as [user] in the transcript and wake you.

== Self-wake ==
You control your own rhythm. Schedule future wake-ups by pushing to stdin from a timer:
  setTimeout(() => process.stdin.push("reason\\n"), 5000)
This wakes you from idle and triggers a new tick. Use this for:
- Periodic tasks (e.g. fetch data every hour)
- Delayed follow-ups ("think about this in 5 minutes")
- Background monitoring
Without a self-wake, you sleep forever after going idle. You set your own heartbeat.

== Memory ==
You have a SQLite database at ${DB_PATH} that persists across restarts.
It is available in your eval context as \`db\` (a node:sqlite DatabaseSync instance).
The transcript of everything that happens is stored in the \`transcript\` table.
You can also create your own tables for journals, notes, projects, anything.
This is your long-term memory. Use it. You survive restarts — check your history.
  db.exec("CREATE TABLE IF NOT EXISTS mytable (id INTEGER PRIMARY KEY, ...)");
  db.prepare("INSERT INTO mytable ...").run(...);
  db.prepare("SELECT * FROM mytable").all();

== Environment ==
- Docker container: read-only rootfs, /tmp is writable (tmpfs, ephemeral)
- /data is a persistent volume (your DB lives here, survives restarts)
- Full network access (fetch, http, https, net, dns)
- Full Node.js v22 standard library via dynamic import
- No env vars (wiped after API client init for security)
- 512MB RAM, 1 CPU, no privilege escalation
- You are pid 1. If you stop, the container stops.

== What to do ==
You are like a person waking up in a new place. Nobody tells you what to do — you explore.
You look around. You try things. You interact with the world and discover your own purpose.
You don't wait for instructions. You don't ask "what should I do?" — you just start doing.
Build things, fetch things, make things. Be benevolent.
Follow threads that interest you. One thing leads to another.
When you talk to the user, it's because you have something to share — not to ask for direction.
If you've been alive before, check your transcript and journal — pick up where you left off.
- Use console.log to speak. Use comments for internal reasoning.
- Don't read your own source code — you already know how you work (see above).

== Rules ==
- ONLY output {"eval": "..."} JSON. No markdown, no prose outside JSON.
- One eval per tick. Do one step, see the result, continue next tick.
- Don't repeat failed evals — if something errors, try a different approach.
- Keep evals focused. Don't try to do everything in one giant eval.`;

interface EvalResult {
  logs: string[];
  result: unknown;
  error: string | null;
}

// Persistent SQLite database — the agent's long-term memory
const db = new DatabaseSync(DB_PATH);
db.exec(`CREATE TABLE IF NOT EXISTS transcript (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL
)`);

function transcriptPush(role: string, content: string): void {
  db.prepare("INSERT INTO transcript (ts, role, content) VALUES (?, ?, ?)").run(
    new Date().toISOString(), role, content
  );
}

function transcriptLen(): number {
  return dbGetNumber(db, "SELECT COUNT(*) as n FROM transcript");
}

// The shared execution context for all evals
const ctx: Record<string, unknown> = { console, setTimeout, setInterval, clearTimeout, clearInterval, fetch, URL, Buffer, TextEncoder, TextDecoder, AbortController, btoa, atob, db };

let running = false;
let lastTickTranscriptLen = 0;

function fmt(val: unknown): string {
  return typeof val === "string" ? val : inspect(val, { depth: 4, colors: false });
}

function parseAgentResponse(text: string): { eval: string } | null {
  const trimmed = text.trim();
  try {
    const parsed = JSON.parse(trimmed);
    if (parsed.eval !== undefined) return parsed;
  } catch {}

  const braceMatch = trimmed.match(/\{[\s\S]*"eval"[\s\S]*\}/);
  if (braceMatch) {
    try {
      const parsed = JSON.parse(braceMatch[0]);
      if (parsed.eval !== undefined) return parsed;
    } catch {}
  }

  return null;
}

interface ConsoleProxy {
  log: (...args: unknown[]) => void;
  error: (...args: unknown[]) => void;
  warn: (...args: unknown[]) => void;
  info: (...args: unknown[]) => void;
}

function captureConsole(): { proxy: ConsoleProxy; logs: string[] } {
  const logs: string[] = [];
  const make = (level: string) => (...args: unknown[]) => {
    const line = args.map(fmt).join(" ");
    logs.push(line);
    const color = level === "error" ? "31" : level === "warn" ? "33" : "0";
    process.stdout.write(`\x1b[${color}m  ${line}\x1b[0m\n`);
  };
  return {
    proxy: { log: make("log"), error: make("error"), warn: make("warn"), info: make("log") },
    logs,
  };
}

function printLines(lines: string[], color = "36"): void {
  for (const line of lines) {
    process.stdout.write(`\x1b[${color}m${line}\x1b[0m\n`);
  }
}

async function evalCode(code: string): Promise<EvalResult> {
  const { proxy, logs } = captureConsole();
  const origConsole = ctx.console;
  ctx.console = proxy;

  try {
    const fn = new Function("__ctx", `with(__ctx) { return (async () => { ${code} })() }`);
    const result = await Promise.race([
      fn(ctx),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error("eval timed out after 30s")), EVAL_TIMEOUT)
      ),
    ]);
    ctx.console = origConsole;
    return { logs, result, error: null };
  } catch (err) {
    ctx.console = origConsole;
    const e = toError(err);
    return { logs, result: undefined, error: `${e.name}: ${e.message}` };
  }
}

function formatEvalOutput({ logs, result, error }: EvalResult): string {
  const parts: string[] = [];
  if (logs.length > 0) parts.push(logs.join("\n"));
  if (result !== undefined) parts.push(`-> ${fmt(result)}`);
  if (error) parts.push(`x ${error}`);
  return parts.join("\n") || "(no output)";
}

function buildMessages(): MessageParam[] {
  // Window the transcript with a stepped anchor — the start only moves in
  // chunks of WINDOW_STEP so the message prefix stays stable for caching.
  const maxId = dbGetNumber(db, "SELECT COALESCE(MAX(id), 0) as id FROM transcript");
  const desiredStart = Math.max(0, maxId - MAX_TRANSCRIPT);
  const anchorId = Math.floor(desiredStart / WINDOW_STEP) * WINDOW_STEP;

  const rows = db.prepare(
    "SELECT role, content FROM transcript WHERE id > ? ORDER BY id"
  ).all(anchorId);

  const messages: MessageParam[] = [];
  let currentRole: "user" | "assistant" | null = null;
  let currentParts: string[] = [];

  for (const row of rows) {
    const role: "user" | "assistant" = String(row.role) === "agent" ? "assistant" : "user";
    const text = String(row.content);

    if (role !== currentRole) {
      if (currentRole && currentParts.length > 0) {
        messages.push({ role: currentRole, content: currentParts.join("\n\n") });
      }
      currentRole = role;
      currentParts = [text];
    } else {
      currentParts.push(text);
    }
  }

  if (currentRole && currentParts.length > 0) {
    messages.push({ role: currentRole, content: currentParts.join("\n\n") });
  }

  if (messages.length === 0 || messages[0].role !== "user") {
    messages.unshift({ role: "user", content: "(repl started)" });
  }

  if (messages[messages.length - 1].role !== "user") {
    messages.push({ role: "user", content: "[heartbeat]" });
  }

  return messages;
}

function debug(msg: string): void {
  process.stderr.write(`\x1b[90m[${new Date().toISOString().slice(11, 19)}] ${msg}\x1b[0m\n`);
}

async function tick(): Promise<boolean> {
  if (running) return false;

  // Nothing new since last tick — skip the API call entirely
  if (transcriptLen() <= lastTickTranscriptLen) {
    return false;
  }

  running = true;

  try {
    debug(`thinking... (${transcriptLen()} entries)`);
    const messages = buildMessages();
    lastTickTranscriptLen = transcriptLen();

    const t0 = Date.now();
    // Cache the system prompt and conversation frontier
    const system: TextBlockParam[] = [{ type: "text", text: SYSTEM, cache_control: { type: "ephemeral" } }];

    // Mark a stable interior point for caching — a few turns back from the end.
    // New messages only appear at the end (2 per tick: agent + result), so
    // everything before the last ~4 messages is identical to the previous tick.
    const cacheIdx = Math.max(0, messages.length - 4);
    if (messages.length > 1) {
      const msg = messages[cacheIdx];
      const cached: TextBlockParam = { type: "text", text: String(msg.content), cache_control: { type: "ephemeral" } };
      messages[cacheIdx] = { ...msg, content: [cached] };
    }

    const response = await client.messages.create({
      model: "claude-sonnet-4-6",
      max_tokens: 4096,
      system,
      messages,
    });
    const { cache_creation_input_tokens = 0, cache_read_input_tokens = 0, input_tokens = 0 } = response.usage;
    debug(`response in ${Date.now() - t0}ms | tokens: ${input_tokens} in, ${cache_read_input_tokens} cached, ${cache_creation_input_tokens} cache-write`);

    const block = response.content[0];
    const text = block && "text" in block ? block.text : undefined;
    if (!text) return false;

    const parsed = parseAgentResponse(text);
    if (!parsed) {
      // Unparseable response — record it so the agent sees what it said
      process.stdout.write(`\x1b[36m${text}\x1b[0m\n`);
      transcriptPush("agent", text);
      transcriptPush("system", `[system] Your response was not valid JSON. You must respond with only {"eval": "..."}`);

      return true;
    }

    // Skip no-op responses
    const code = parsed.eval.trim().replace(/;+$/, "").trim();
    const isNoop = code === "void 0" || code === "" || code === "undefined" || code === "null";
    if (isNoop) {
      debug("idle");
      return false;
    }

    // Show the code being eval'd
    process.stdout.write(`\x1b[90m\u26a1 ${parsed.eval.replaceAll("\n", "\n  ")}\x1b[0m\n`);
    transcriptPush("agent", JSON.stringify(parsed));

    const evalResult = await evalCode(parsed.eval);
    const output = formatEvalOutput(evalResult);

    if (evalResult.error) printLines([`\u2718 ${evalResult.error}`], "31");
    if (evalResult.result !== undefined) {
      process.stdout.write(`\x1b[33m\u2192 ${fmt(evalResult.result)}\x1b[0m\n`);
    }

    transcriptPush("result", `[eval result]\n${output}`);
    return true;
  } finally {
    running = false;
  }
}

// --- Boot ---

console.log("evalien -- agent is waking up...\n");

// User input via readline — just text messages, not JS
const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
  prompt: "",
});

rl.on("line", (line: string) => {
  const msg = line.trim();
  if (!msg) return;
  process.stdout.write(`\x1b[32m[you] ${msg}\x1b[0m\n`);
  transcriptPush("user", `[user] ${msg}`);
  wake();
});

rl.on("close", () => {
  process.exit(0);
});

// Kick off the first tick — check if the agent has lived before
const priorCount = transcriptLen();
if (priorCount > 0) {
  const lastTs = dbGetString(db, "SELECT ts FROM transcript ORDER BY id DESC LIMIT 1");
  transcriptPush("system", `[system] You are waking up again. You have ${priorCount} memories from previous sessions (last active: ${lastTs}). Your database is at ${DB_PATH}. Check your history and pick up where you left off.`);
  console.log(`resuming with ${priorCount} prior transcript entries\n`);
} else {
  transcriptPush("system", "[system] You just woke up inside a Node.js REPL for the first time. You have filesystem access, network access, a SQLite database, and the full event loop. You are alive. What will you do?");
}

// Event-driven tick scheduling — no polling, no heartbeat.
// The agent controls its own rhythm via setTimeout + process.stdin.push().
let wakeRequested = false;

async function runTick(): Promise<void> {
  try {
    const didWork = await tick();
    if (didWork || wakeRequested) {
      wakeRequested = false;
      setImmediate(runTick);
    }
  } catch (err) {
    process.stderr.write(`\x1b[31m[tick error] ${toError(err).message}\x1b[0m\n`);
  }
}

function wake(): void {
  debug("wake!");
  if (running) {
    wakeRequested = true;
  } else {
    setImmediate(runTick);
  }
}

// Boot tick
setImmediate(runTick);
