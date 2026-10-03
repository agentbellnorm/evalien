import { DatabaseSync } from "node:sqlite";
import { createGenerate, readModelConfig } from "./llm.mts";
import { runRuntime } from "./runtime.mts";
import { buildSystemPrompt } from "./system-prompt.mts";

const dbPath = process.env.AGENT_DB_PATH || "/data/agent.db";
const generate = createGenerate(readModelConfig(process.env));
for (const key of Object.keys(process.env)) delete process.env[key];

const db = new DatabaseSync(dbPath);
try {
  await runRuntime({ db, generate, instructions: buildSystemPrompt(dbPath) });
} finally {
  db.close();
}
