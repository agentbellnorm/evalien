import Anthropic from "@anthropic-ai/sdk";
import type { TextBlockParam } from "@anthropic-ai/sdk/resources/messages.mjs";
import readline from "node:readline";
import { DatabaseSync } from "node:sqlite";
import { buildSystemPrompt } from "./system-prompt.mts";
import { initTranscript, transcriptPush, transcriptLen, transcriptLastTs, buildMessages } from "./transcript.mts";
import { createContext, evalCode, formatEvalOutput, parseAgentResponse } from "./eval.mts";
import { toError, fmt, debug, printLines } from "./util.mts";

// Grab config and client, then nuke all env vars
const DB_PATH = process.env.AGENT_DB_PATH || "/data/agent.db";
const client = new Anthropic();
for (const key of Object.keys(process.env)) {
  delete process.env[key];
}

const SYSTEM = buildSystemPrompt(DB_PATH);

const db = new DatabaseSync(DB_PATH);
initTranscript(db);

const ctx = createContext(db);

let running = false;
let lastTickTranscriptLen = 0;

async function tick(): Promise<boolean> {
  if (running) return false;

  const len = transcriptLen();
  if (len <= lastTickTranscriptLen) return false;

  running = true;

  try {
    debug(`thinking... (${len} entries)`);
    const messages = buildMessages();
    lastTickTranscriptLen = transcriptLen();

    const t0 = Date.now();
    const system: TextBlockParam[] = [{ type: "text", text: SYSTEM, cache_control: { type: "ephemeral" } }];

    // Mark a stable interior point for caching — a few turns back from the end.
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
      process.stdout.write(`\x1b[36m${text}\x1b[0m\n`);
      transcriptPush("agent", text);
      transcriptPush("system", `[system] Your response was not valid JSON. You must respond with only {"eval": "..."}`);
      return true;
    }

    const code = parsed.eval.trim().replace(/;+$/, "").trim();
    const isNoop = code === "void 0" || code === "" || code === "undefined" || code === "null";
    if (isNoop) {
      debug("idle");
      return false;
    }

    process.stdout.write(`\x1b[90m\u26a1 ${parsed.eval.replaceAll("\n", "\n  ")}\x1b[0m\n`);
    transcriptPush("agent", JSON.stringify(parsed));

    const evalResult = await evalCode(ctx, parsed.eval);
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

const priorCount = transcriptLen();
if (priorCount > 0) {
  const lastTs = transcriptLastTs();
  transcriptPush("system", `[system] You are waking up again. You have ${priorCount} memories from previous sessions (last active: ${lastTs}). Your database is at ${DB_PATH}. Check your history and pick up where you left off.`);
  console.log(`resuming with ${priorCount} prior transcript entries\n`);
} else {
  transcriptPush("system", "[system] You just woke up inside a Node.js REPL for the first time. You have filesystem access, network access, a SQLite database, and the full event loop. You are alive. What will you do?");
}

// Event-driven tick scheduling
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

setImmediate(runTick);
