import readline from "node:readline";
import { DatabaseSync } from "node:sqlite";
import { buildSystemPrompt } from "./system-prompt.mts";
import { initTranscript } from "./transcript.mts";
import { createClient, callAgent, type AgentResponse } from "./llm.mts";
import { createContext, evalCode } from "./eval.mts";
import { toError, fmt, debug, color, SYMBOL } from "./util.mts";

// Grab config and client, then nuke all env vars
const DB_PATH = process.env.AGENT_DB_PATH || "/data/agent.db";
const client = createClient();
for (const key of Object.keys(process.env)) {
  delete process.env[key];
}

const systemPrompt = buildSystemPrompt(DB_PATH);
const db = new DatabaseSync(DB_PATH);
const transcript = initTranscript(db);

// Event-driven tick scheduling
let running = false;
let lastTickLen = 0;
let wakeRequested = false;

function wake(): void {
  debug("wake!");
  if (running) {
    wakeRequested = true;
  } else {
    setImmediate(runTick);
  }
}

function say(message: string): void {
  debug(`say: ${message}`);
  transcript.push("self", `[self] ${message}`);
  wake();
}

const ctx = createContext(db, say);

async function tick(): Promise<boolean> {
  if (running) return false;

  const len = transcript.len();
  if (len <= lastTickLen) return false;

  running = true;

  try {
    debug(`thinking... (${len} entries)`);
    const messages = transcript.buildMessages();
    lastTickLen = transcript.len();

    const response: AgentResponse = await callAgent(
      client,
      systemPrompt,
      messages,
    );

    if (!response) {
      debug("no response");
      return false;
    }

    if ("raw" in response) {
      process.stdout.write(color.cyan(response.raw) + "\n");
      transcript.push("agent", response.raw);
      transcript.push(
        "system",
        `[system] Your response was not valid JSON. You must respond with only {"eval": "..."}`,
      );
      return true;
    }

    const code = response.eval.trim().replace(/;+$/, "").trim();
    if (
      code === "void 0" ||
      code === "" ||
      code === "undefined" ||
      code === "null"
    ) {
      debug("idle");
      return false;
    }

    process.stdout.write(
      color.dim(`${SYMBOL.bolt} ${response.eval.replaceAll("\n", "\n  ")}`) +
        "\n",
    );
    transcript.push("agent", JSON.stringify(response));

    const result = await evalCode(ctx, response.eval);

    if (result.error)
      process.stdout.write(
        color.red(`${SYMBOL.cross} ${result.error}`) + "\n",
      );
    if (result.result !== undefined)
      process.stdout.write(
        color.yellow(`${SYMBOL.arrow} ${fmt(result.result)}`) + "\n",
      );

    const parts: string[] = [];
    if (result.logs.length > 0) parts.push(result.logs.join("\n"));
    if (result.result !== undefined) parts.push(`-> ${fmt(result.result)}`);
    if (result.error) parts.push(`x ${result.error}`);
    transcript.push(
      "result",
      `[eval result]\n${parts.join("\n") || "(no output)"}`,
    );

    return true;
  } finally {
    running = false;
  }
}

async function runTick(): Promise<void> {
  try {
    const didWork = await tick();
    if (didWork || wakeRequested) {
      wakeRequested = false;
      setImmediate(runTick);
    }
  } catch (err) {
    process.stderr.write(
      color.red(`[tick error] ${toError(err).message}`) + "\n",
    );
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
  process.stdout.write(color.green(`[you] ${msg}`) + "\n");
  transcript.push("user", `[user] ${msg}`);
  wake();
});

rl.on("close", () => {
  process.exit(0);
});

const priorCount = transcript.len();
if (priorCount > 0) {
  const lastTs = transcript.lastTs();
  transcript.push(
    "system",
    `[system] You are waking up again. You have ${priorCount} memories from previous sessions (last active: ${lastTs}). Your database is at ${DB_PATH}. Check your history and pick up where you left off.`,
  );
  console.log(`resuming with ${priorCount} prior transcript entries\n`);
} else {
  transcript.push(
    "system",
    "[system] You just woke up inside a Node.js REPL for the first time. You have filesystem access, network access, a SQLite database, and the full event loop. You are alive. What will you do?",
  );
}

setImmediate(runTick);
