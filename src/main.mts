import { DatabaseSync } from "node:sqlite";
import { createGenerate } from "./inference/ai-sdk.mts";
import { BudgetExceededError, withBudget } from "./inference/budget.mts";
import { readInferenceConfig } from "./inference/config.mts";
import { writeStderr } from "./output.mts";
import { debug } from "./util.mts";
import { createSqliteStore } from "./db/sqlite-store.mts";
import { runRuntime } from "./harness/runtime.mts";
import { createTrajectory } from "./trajectory/log.mts";
import { buildSystemPrompt } from "./system-prompt.mts";

const dbPath = process.env.AGENT_DB_PATH || "/data/agent.db";
const config = readInferenceConfig(process.env);
const { generate } = withBudget(createGenerate(config, { warn: debug }), config.budgetUSD);
for (const key of Object.keys(process.env)) delete process.env[key];

const db = new DatabaseSync(dbPath);
const log = createTrajectory(createSqliteStore(db, "trajectory"));

// As PID 1 in a container, the process has no default signal handling, so
// Ctrl+C and `podman stop` only work through explicit handlers.
for (const [signal, code] of [["SIGINT", 130], ["SIGTERM", 143]] as const) {
  process.on(signal, () => {
    writeStderr(`\n${signal}: stopping.\n`);
    if (db.isOpen) db.close();
    process.exit(code);
  });
}

let exitCode = 0;
try {
  await runRuntime({ log, db, generate, instructions: buildSystemPrompt(dbPath) });
} catch (err) {
  if (!(err instanceof BudgetExceededError)) throw err;
  writeStderr(`\n${err.message}. Stopping.\n`);
  exitCode = 2;
} finally {
  if (db.isOpen) db.close();
}
// Timers left by evaluated code would otherwise keep a stopped runtime alive.
process.exit(exitCode);
