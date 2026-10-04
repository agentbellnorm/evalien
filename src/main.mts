import { DatabaseSync } from "node:sqlite";
import { createSqliteStore } from "./db/sqlite-store.mts";
import { createEvaluator } from "./evaluation/node-eval.mts";
import { runRuntime } from "./harness/runtime.mts";
import { createGenerate } from "./inference/ai-sdk.mts";
import { BudgetExceededError, withBudget } from "./inference/budget.mts";
import { readInferenceConfig } from "./inference/config.mts";
import { buildSystemPrompt } from "./system-prompt.mts";
import { captureOutput, writeStderr, writeStdout } from "./terminal/capture.mts";
import { showTrajectory } from "./terminal/display.mts";
import { debug } from "./terminal/format.mts";
import { lineInput } from "./terminal/input.mts";
import { createTrajectory } from "./trajectory/log.mts";

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

showTrajectory(log);
writeStdout(`evalien -- agent is waking up (${log.lastSeq()} prior events)...\n\n`);
let exitCode = 0;
try {
  await runRuntime({
    log,
    generate,
    // The agent's own handle on the database, including its trajectory.
    evaluator: createEvaluator({ db }),
    input: lineInput(),
    output: captureOutput,
    instructions: buildSystemPrompt(dbPath),
    debug,
  });
} catch (err) {
  if (!(err instanceof BudgetExceededError)) throw err;
  writeStderr(`\n${err.message}. Stopping.\n`);
  exitCode = 2;
} finally {
  if (db.isOpen) db.close();
}
// Timers left by evaluated code would otherwise keep a stopped runtime alive.
process.exit(exitCode);
