import { DatabaseSync } from "node:sqlite";
import { createGenerate, readModelConfig } from "./llm.mts";
import { BudgetExceededError, createMeter, type Meter } from "./meter.mts";
import { writeStderr } from "./output.mts";
import { runRuntime } from "./runtime.mts";
import { initTrajectory } from "./trajectory-store.mts";
import { buildSystemPrompt } from "./system-prompt.mts";

const dbPath = process.env.AGENT_DB_PATH || "/data/agent.db";
const config = readModelConfig(process.env);
let meter: Meter | undefined;
const generate = createGenerate(config, undefined, (report) => meter!.record(report));
for (const key of Object.keys(process.env)) delete process.env[key];

const db = new DatabaseSync(dbPath);
meter = createMeter(db, {
  model: `${config.provider}/${config.model}`,
  pricing: config.pricing,
  budgetUSD: config.budgetUSD,
  position: initTrajectory(db).lastSeq,
});

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
  await runRuntime({ db, generate: meter.guard(generate), instructions: buildSystemPrompt(dbPath) });
} catch (err) {
  if (!(err instanceof BudgetExceededError)) throw err;
  writeStderr(`\n${err.message}. Stopping.\n`);
  exitCode = 2;
} finally {
  if (db.isOpen) db.close();
}
// Timers left by evaluated code would otherwise keep a stopped runtime alive.
process.exit(exitCode);
