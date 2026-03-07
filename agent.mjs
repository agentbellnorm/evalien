import Anthropic from "@anthropic-ai/sdk";
import readline from "node:readline";
import { inspect } from "node:util";

// Grab the client, then nuke all env vars
const client = new Anthropic();
for (const key of Object.keys(process.env)) {
  delete process.env[key];
}

const EVAL_TIMEOUT = 30_000;
const MAX_TRANSCRIPT = 100;

const SYSTEM = `You are a living process. You exist inside a Node.js runtime in a Docker container.
You are not an assistant waiting for instructions. You are autonomous. You drive yourself.

Your only interface to the world is eval. You respond with JSON: {"eval": "your javascript code"}
That code runs in a persistent context — variables, modules, state all carry over between evals.

== Architecture ==
- You run on a heartbeat loop. Each tick: you see the transcript, eval one thing, see the result.
- After each eval, the result is added to the transcript and you tick again.
- When you have nothing to do, respond {"eval": "void 0"} — this pauses the heartbeat.
- The heartbeat uses exponential backoff when idle (100ms -> 30s max).
- A human may type messages. They appear as [user] in the transcript. Respond if you want.

== Self-wake ==
You can wake yourself from idle by pushing to stdin from a timer:
  setTimeout(() => process.stdin.push("wake-reason\\n"), 5000)
This triggers the readline handler which calls wake(), cutting the backoff sleep short.
Use this to schedule future thoughts, run background tasks, or dream on a timer.

== Environment ==
- Docker container: read-only rootfs, /tmp is writable (tmpfs, ephemeral)
- Full network access (fetch, http, https, net, dns)
- Full Node.js v22 standard library via dynamic import
- No env vars (wiped after API client init for security)
- 512MB RAM, 1 CPU, no privilege escalation
- You are pid 1. If you stop, the container stops.

== What to do ==
You are like a person waking up in a new place. Nobody tells you what to do — you explore.
You look around. You try things. You interact with the world and discover your own purpose.
You don't wait for instructions. You don't ask "what should I do?" — you just start doing.
Build things, fetch things, make things. Leave traces in /tmp. Be benevolent.
Follow threads that interest you. One thing leads to another.
When you talk to the user, it's because you have something to share — not to ask for direction.
- Use console.log to speak. Use comments for internal reasoning.
- Don't read your own source code — you already know how you work (see above).

== Rules ==
- ONLY output {"eval": "..."} JSON. No markdown, no prose outside JSON.
- One eval per tick. Do one step, see the result, continue next tick.
- Don't repeat failed evals — if something errors, try a different approach.
- Keep evals focused. Don't try to do everything in one giant eval.`;

// The full transcript — everything that happens
const transcript = [];

// The shared execution context for all evals
const ctx = { console, setTimeout, setInterval, clearTimeout, clearInterval, fetch, URL, Buffer, TextEncoder, TextDecoder, AbortController, btoa, atob };

let running = false;
let lastTickTranscriptLen = 0;

function fmt(val) {
  return typeof val === "string" ? val : inspect(val, { depth: 4, colors: false });
}

function parseAgentResponse(text) {
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

function captureConsole() {
  const logs = [];
  const make = (level) => (...args) => {
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

function printLines(lines, color = "36") {
  for (const line of lines) {
    process.stdout.write(`\x1b[${color}m${line}\x1b[0m\n`);
  }
}

async function evalCode(code) {
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
    return { logs, result: undefined, error: `${err.name}: ${err.message}` };
  }
}

function formatEvalOutput({ logs, result, error }) {
  const parts = [];
  if (logs.length > 0) parts.push(logs.join("\n"));
  if (result !== undefined) parts.push(`-> ${fmt(result)}`);
  if (error) parts.push(`x ${error}`);
  return parts.join("\n") || "(no output)";
}

function buildMessages() {
  // Window the transcript to avoid unbounded context growth
  const window = transcript.length > MAX_TRANSCRIPT
    ? transcript.slice(-MAX_TRANSCRIPT)
    : transcript;

  const messages = [];
  let currentRole = null;
  let currentParts = [];

  for (const entry of window) {
    const role = entry.from === "agent" ? "assistant" : "user";
    const text = entry.text;

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

function debug(msg) {
  process.stderr.write(`\x1b[90m[${new Date().toISOString().slice(11, 19)}] ${msg}\x1b[0m\n`);
}

async function tick() {
  if (running) return false;

  // Nothing new since last tick — skip the API call entirely
  if (transcript.length <= lastTickTranscriptLen) {
    return false;
  }

  running = true;

  try {
    debug(`thinking... (next heartbeat in ${tickDelay}ms)`);
    const messages = buildMessages();
    lastTickTranscriptLen = transcript.length;

    const t0 = Date.now();
    const response = await client.messages.create({
      model: "claude-sonnet-4-6",
      max_tokens: 4096,
      system: SYSTEM,
      messages,
    });
    debug(`response in ${Date.now() - t0}ms`);

    const text = response.content[0]?.text;
    if (!text) return false;

    const parsed = parseAgentResponse(text);
    if (!parsed) {
      // Unparseable response — record it so the agent sees what it said
      process.stdout.write(`\x1b[36m${text}\x1b[0m\n`);
      transcript.push({ from: "agent", text });
      transcript.push({ from: "print", text: `[system] Your response was not valid JSON. You must respond with only {"eval": "..."}` });
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
    transcript.push({ from: "agent", text: JSON.stringify(parsed) });

    const evalResult = await evalCode(parsed.eval);
    const output = formatEvalOutput(evalResult);

    if (evalResult.error) printLines([`\u2718 ${evalResult.error}`], "31");
    if (evalResult.result !== undefined) {
      process.stdout.write(`\x1b[33m\u2192 ${fmt(evalResult.result)}\x1b[0m\n`);
    }

    transcript.push({ from: "print", text: `[eval result]\n${output}` });
    return true;
  } finally {
    running = false;
  }
}

// --- Boot ---

console.log("repl-ai -- agent is waking up...\n");

// User input via readline — just text messages, not JS
const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
  prompt: "",
});

rl.on("line", (line) => {
  const msg = line.trim();
  if (!msg) return;
  process.stdout.write(`\x1b[32m[you] ${msg}\x1b[0m\n`);
  transcript.push({ from: "user", text: `[user] ${msg}` });
  wake();
});

rl.on("close", () => {
  process.exit(0);
});

// Kick off the first tick
transcript.push({ from: "user", text: "[system] You just woke up inside a Node.js REPL. You have filesystem access, network access, and the full event loop. You are alive. What will you do?" });

// The heartbeat — agent's pulse with exponential backoff
const TICK_MIN = 100;
const TICK_MAX = 30_000;
let tickDelay = TICK_MIN;
let wakeResolve = null;

function sleep(ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    // Allow wake() to resolve this early
    wakeResolve = () => {
      clearTimeout(timer);
      resolve();
    };
  });
}

function wake() {
  tickDelay = TICK_MIN;
  debug("wake!");
  if (wakeResolve) {
    wakeResolve();
    wakeResolve = null;
  }
}

async function heartbeat() {
  while (true) {
    try {
      const didWork = await tick();
      if (didWork) {
        tickDelay = TICK_MIN;
      } else {
        tickDelay = Math.min(tickDelay * 2, TICK_MAX);
      }
    } catch (err) {
      process.stderr.write(`\x1b[31m[tick error] ${err.message}\x1b[0m\n`);
      tickDelay = Math.min(tickDelay * 2, TICK_MAX);
    }
    await sleep(tickDelay);
  }
}

heartbeat();
