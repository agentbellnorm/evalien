// The agent's composition root: which implementation fills each contract.
import { readConfig, TRAJECTORY_TABLE } from "./config.mts";
import { openDatabase } from "./db/open.mts";
import { createSqliteStore } from "./db/sqlite-store.mts";
import { createEvaluator } from "./evaluation/node-eval.mts";
import { IDLE_DELAYS } from "./harness/policy.mts";
import { runRuntime } from "./harness/runtime.mts";
import { createGenerate } from "./inference/ai-sdk.mts";
import { BudgetExceededError, withBudget } from "./inference/budget.mts";
import { runUntilExit, takeEnvironment } from "./lifecycle/process.mts";
import { buildSystemPrompt } from "./system-prompt.mts";
import { captureOutput } from "./terminal/capture.mts";
import { showTrajectory } from "./terminal/display.mts";
import { debug } from "./terminal/format.mts";
import { lineInput } from "./terminal/input.mts";
import { createTrajectory } from "./trajectory/log.mts";

const { dbPath, inference } = readConfig(takeEnvironment());

const db = openDatabase(dbPath);
const log = createTrajectory(createSqliteStore(db, TRAJECTORY_TABLE));
const { generate } = withBudget(createGenerate(inference, { warn: debug }), inference.budgetUSD);
showTrajectory(log);

await runUntilExit(() => runRuntime({
  log,
  generate,
  // The agent's own handle on the database, including its trajectory.
  evaluator: createEvaluator({ db }),
  input: lineInput(),
  output: captureOutput,
  instructions: buildSystemPrompt({ dbPath, table: TRAJECTORY_TABLE, idleDelays: IDLE_DELAYS }),
  idleDelays: IDLE_DELAYS,
  debug,
}), {
  cleanup: () => db.close(),
  exitCodes: [[BudgetExceededError, 2]],
});
