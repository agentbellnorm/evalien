import assert from "node:assert/strict";
import { test } from "node:test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import { createMeter, type GenerationReport } from "../src/meter.mts";
import { analyze, readRun, renderTimeline } from "../src/report.mts";
import type { TrajectoryEvent } from "../src/contracts.mts";
import { createSqliteStore } from "../src/db/sqlite-store.mts";
import { createTrajectory } from "../src/trajectory/log.mts";

const usage: GenerationReport = {
  latencyMs: 0, finishReason: "stop", uncachedInputTokens: 100, cacheReadTokens: 900,
  cacheWriteTokens: 0, outputTokens: 50, reasoningTokens: 0,
};

/** Record a run as the runtime does: each model call is logged just before its eval. */
async function record(db: DatabaseSync, steps: (TrajectoryEvent | "generate" | "idle")[]): Promise<void> {
  const trajectory = createTrajectory(createSqliteStore(db, "trajectory"));
  const meter = createMeter(db, {
    model: "anthropic/test", pricing: { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2.5 }, budgetUSD: 1,
    position: trajectory.lastSeq,
  });
  const generate = meter.guard(async () => { meter.record(usage); return ""; });
  let lastEval = 0;
  for (const step of steps) {
    if (step === "generate" || step === "idle") await generate({ instructions: "", blocks: [""] });
    else if (step.type === "result") trajectory.append({ ...step, evalId: lastEval });
    else {
      const entry = trajectory.append(step);
      if (step.type === "eval") lastEval = entry.seq;
    }
  }
}

const result = (outcome: "return" | "throw", text: string): TrajectoryEvent =>
  ({ type: "result", evalId: 0, outcome, text });

test("a healthy run passes every check and interleaves model calls with the trajectory", async () => {
  const db = new DatabaseSync(":memory:");
  await record(db, [
    { type: "start" },
    "generate", { type: "eval", code: "return 1;" }, result("return", "1"),
    { type: "stdin", text: "hi" },
    "generate", { type: "eval", code: 'console.log("hello")' }, { type: "stdout", text: "hello\n" }, result("return", "undefined"),
    "generate", { type: "eval", code: "JSON.parse('{')" }, result("throw", "SyntaxError: bad JSON"),
  ]);
  const report = analyze(...Object.values(readRun(db)) as Parameters<typeof analyze>);
  assert.deepEqual(report.checks.map((check) => [check.name, check.pass]), [
    ["model output parses as JavaScript", true],
    ["human input reaches the model promptly", true],
    ["prompt cache is reused", true],
  ]);
  // A thrown SyntaxError at runtime is the program's business, not malformed output.
  assert.match(report.checks[0].detail, /^0\/3 malformed/);
  // 100 × $2 + 900 × $0.20 + 50 × $10 per million is $0.00088 per call.
  assert.equal(report.summary.cost, "$0.0026 ($0.0009 per call)");

  const timeline = renderTimeline(report);
  const order = [...timeline.matchAll(/^\[(model|\d+ \S+ (\w+))/gm)].map((m) => m[2] ?? m[1]);
  assert.deepEqual(order, [
    "start", "model", "eval", "result", "stdin", "model", "eval", "stdout", "result", "model", "eval", "result",
  ]);
});

test("idle calls are counted and placed after what they saw", async () => {
  const db = new DatabaseSync(":memory:");
  await record(db, [
    { type: "start" },
    "generate", { type: "eval", code: "return 1;" }, result("return", "1"),
    "idle",
    { type: "stdin", text: "hi" },
    "generate", { type: "eval", code: "return 2;" }, result("return", "2"),
  ]);
  const report = analyze(...Object.values(readRun(db)) as Parameters<typeof analyze>);
  assert.equal(report.summary.generations, "3 (1 idle, 0 incomplete)");
  const order = [...renderTimeline(report).matchAll(/^\[(model|\d+ \S+ (\w+)).*?(, idle)?$/gm)]
    .map((m) => (m[2] ?? m[1]) + (m[3] ?? ""));
  assert.deepEqual(order, ["start", "model", "eval", "result", "model, idle", "stdin", "model", "eval", "result"]);
});

test("tool-call markup and unanswered input fail the run", async () => {
  const db = new DatabaseSync(":memory:");
  await record(db, [
    { type: "start" },
    "generate", { type: "eval", code: '<invoke name="eval">\n</invoke>\n```js\nreturn 1;\n```' }, result("throw", "SyntaxError: Unexpected token '<'"),
    "generate", { type: "eval", code: "await new Promise(r => setTimeout(r, 600000));" },
    { type: "stdin", text: "hello?" },
  ]);
  const report = analyze(...Object.values(readRun(db)) as Parameters<typeof analyze>);
  const [parses, input] = report.checks;
  assert.equal(parses.pass, false);
  assert.match(parses.detail, /1\/2 malformed \(50\.0%.*seq 2; 1 contain tool-call markup or fences/);
  assert.equal(input.pass, false);
  assert.match(input.detail, /seq 5: never seen/);
});

test("--last scopes the report to the latest start, and failures exit nonzero", async () => {
  const dir = mkdtempSync(join(tmpdir(), "evalien-report-"));
  try {
    const path = join(dir, "agent.db");
    const db = new DatabaseSync(path);
    await record(db, [
      { type: "start" }, "generate", { type: "eval", code: "<oops>" }, result("throw", "SyntaxError"),
      { type: "start" }, "generate", { type: "eval", code: "return 2;" }, result("return", "2"),
    ]);
    db.close();
    const cli = fileURLToPath(new URL("../src/report.mts", import.meta.url));
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
    assert.match(last.out, /\[model .* anthropic\/test through=4\] 0ms, stop/);
    assert.equal(run("--bogus").code, 64);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
