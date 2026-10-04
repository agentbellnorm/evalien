import assert from "node:assert/strict";
import { test } from "node:test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import type { TrajectoryEvent } from "../src/contracts.mts";
import { createSqliteStore } from "../src/db/sqlite-store.mts";
import { analyze, readRun } from "../src/report/report.mts";
import { createTrajectory } from "../src/trajectory/log.mts";

// 100 × $2 + 900 × $0.20 + 50 × $10 per million is $0.00088 per call.
const usage = { uncachedInputTokens: 100, cacheReadTokens: 900, cacheWriteTokens: 0, outputTokens: 50, reasoningTokens: 0, costUSD: 0.00088 };

/** Record a run as the runtime does: each call is appended right before its eval. */
function record(db: DatabaseSync, steps: (TrajectoryEvent | "generate")[]): void {
  const trajectory = createTrajectory(createSqliteStore(db, "trajectory"));
  let lastEval = 0;
  for (const step of steps) {
    if (step === "generate") {
      trajectory.append({ type: "generation", through: trajectory.lastSeq(), model: "anthropic/test", latencyMs: 0, finishReason: "stop", usage });
    } else if (step.type === "result") trajectory.append({ ...step, evalId: lastEval });
    else {
      const entry = trajectory.append(step);
      if (step.type === "eval") lastEval = entry.seq;
    }
  }
}

const result = (outcome: "return" | "throw", text: string): TrajectoryEvent =>
  ({ type: "result", evalId: 0, outcome, text });

test("a healthy run passes every check", () => {
  const db = new DatabaseSync(":memory:");
  record(db, [
    { type: "start" },
    "generate", { type: "eval", code: "return 1;" }, result("return", "1"),
    { type: "stdin", text: "hi" },
    "generate", { type: "eval", code: 'console.log("hello")' }, { type: "stdout", text: "hello\n" }, result("return", "undefined"),
    "generate", { type: "eval", code: "JSON.parse('{')" }, result("throw", "SyntaxError: bad JSON"),
  ]);
  const report = analyze(readRun(db));
  assert.deepEqual(report.checks.map((check) => [check.name, check.pass]), [
    ["model output parses as JavaScript", true],
    ["human input reaches the model promptly", true],
    ["prompt cache is reused", true],
  ]);
  // A thrown SyntaxError at runtime is the program's business, not malformed output.
  assert.match(report.checks[0].detail, /^0\/3 malformed/);
  assert.equal(report.summary.cost, "$0.0026 ($0.0009 per call)");
  assert.equal(report.summary.generations, "3 (0 idle, 0 incomplete)");
});

test("idle and incomplete calls are told apart", () => {
  const db = new DatabaseSync(":memory:");
  record(db, [{ type: "start" }, "generate", { type: "eval", code: "return 1;" }, result("return", "1"), "generate", { type: "stdin", text: "hi" }]);
  const trajectory = createTrajectory(createSqliteStore(db, "trajectory"));
  trajectory.append({ type: "generation", through: 6, model: "anthropic/test", latencyMs: 0, finishReason: "length", usage });
  assert.equal(analyze(readRun(db)).summary.generations, "3 (1 idle, 1 incomplete)");
});

test("tool-call markup and unanswered input fail the run", () => {
  const db = new DatabaseSync(":memory:");
  record(db, [
    { type: "start" },
    "generate", { type: "eval", code: '<invoke name="eval">\n</invoke>\n```js\nreturn 1;\n```' }, result("throw", "SyntaxError: Unexpected token '<'"),
    "generate", { type: "eval", code: "await new Promise(r => setTimeout(r, 600000));" },
    { type: "stdin", text: "hello?" },
  ]);
  const [parses, input] = analyze(readRun(db)).checks;
  assert.equal(parses.pass, false);
  assert.match(parses.detail, /1\/2 malformed \(50\.0%.*seq 3; 1 contain tool-call markup or fences/);
  assert.equal(input.pass, false);
  assert.match(input.detail, /seq 7: never seen/);
});

test("--last scopes the report to the latest start, and failures exit nonzero", () => {
  const dir = mkdtempSync(join(tmpdir(), "evalien-report-"));
  try {
    const path = join(dir, "agent.db");
    const db = new DatabaseSync(path);
    record(db, [
      { type: "start" }, "generate", { type: "eval", code: "<oops>" }, result("throw", "SyntaxError"),
      { type: "start" }, "generate", { type: "eval", code: "return 2;" }, result("return", "2"),
    ]);
    db.close();
    const cli = fileURLToPath(new URL("../src/report/report.mts", import.meta.url));
    const run = (...args: string[]) => {
      try {
        return { code: 0, out: execFileSync(process.execPath, ["--no-warnings", cli, path, ...args], { encoding: "utf8" }) };
      } catch (err) {
        const { status, stdout } = err as { status: number; stdout: string };
        return { code: status, out: stdout };
      }
    };
    const whole = run();
    assert.equal(whole.code, 1);
    assert.match(whole.out, /FAIL {2}model output parses/);
    const last = run("--last", "--timeline");
    assert.equal(last.code, 0, last.out);
    assert.match(last.out, /evaluations +1\n/);
    // The timeline is the trajectory as the model sees it, calls included.
    assert.match(last.out, /\[6 \S+ generation\]\n\| anthropic\/test through=5: 0\.0s, stop/);
    assert.equal(run("--bogus").code, 64);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
