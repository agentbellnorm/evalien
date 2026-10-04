import assert from "node:assert/strict";
import { test } from "node:test";
import type { TrajectoryEvent } from "../src/contracts.mts";
import { fromHuman, IDLE_DELAYS, idleDelay, wakes } from "../src/harness/policy.mts";

const usage = { uncachedInputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, outputTokens: 0, reasoningTokens: 0, costUSD: 0 };
const events: Record<TrajectoryEvent["type"], TrajectoryEvent> = {
  start: { type: "start" },
  eval: { type: "eval", code: "1" },
  stdin: { type: "stdin", text: "hi" },
  stdout: { type: "stdout", text: "out" },
  stderr: { type: "stderr", text: "err" },
  result: { type: "result", evalId: 1, outcome: "return", text: "1" },
  generation: { type: "generation", through: 1, model: "m", latencyMs: 0, finishReason: "stop", usage },
};

test("which observations wake the model, with and without pending evaluations", () => {
  const table = Object.entries(events).map(([type, event]) => [type, wakes(event, 0), wakes(event, 2)]);
  assert.deepEqual(table, [
    ["start", true, true],
    ["eval", false, false],
    ["stdin", true, true],
    ["stdout", true, false],
    ["stderr", true, false],
    ["result", true, true],
    ["generation", false, false],
  ]);
});

test("only a fresh start or human input resets pacing", () => {
  assert.deepEqual(Object.values(events).filter(fromHuman).map((event) => event.type), ["start", "stdin"]);
});

test("quiet calls get further apart, capped at the last delay", () => {
  assert.deepEqual([0, 1, 2, 3, 4, 5, 6, 7, 50].map((quiet) => idleDelay(quiet)), [0, 0, 0, 0, 5_000, 30_000, 120_000, 600_000, 600_000]);
  assert.equal(IDLE_DELAYS.length, 7);
  assert.equal(idleDelay(3, []), 0);
  assert.equal(idleDelay(5, [10, 20]), 20);
});
