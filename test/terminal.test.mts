import assert from "node:assert/strict";
import { test } from "node:test";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import { captureOutput, writeStdout, writeStderr } from "../src/terminal/capture.mts";
import { createTrajectory } from "../src/trajectory/log.mts";
import { createMemoryStore } from "../src/trajectory/memory-store.mts";
import { createEvaluator } from "../src/evaluation/node-eval.mts";
import { showTrajectory } from "../src/terminal/display.mts";
import type { OutputEvent } from "../src/contracts.mts";

// The Node test runner itself writes a binary protocol to stdout. Exercise
// process-wide capture in a process that has no test reporter attached.
function isolated(check: () => void | Promise<void>): void {
  execFileSync(process.execPath, ["--no-warnings", "--input-type=module", "--eval", `
    import assert from 'node:assert/strict';
    import { DatabaseSync } from 'node:sqlite';
    import { captureOutput, writeStdout, writeStderr } from './src/terminal/capture.mts';
    import { createTrajectory } from './src/trajectory/log.mts';
    import { createMemoryStore } from './src/trajectory/memory-store.mts';
    import { createEvaluator } from './src/evaluation/node-eval.mts';
    await (${check.toString()})();
  `], { cwd: fileURLToPath(new URL("../", import.meta.url)), stdio: "pipe" });
}

test("capture preserves channels, split UTF-8, write callbacks and backpressure", () => isolated(() => {
  const originalOut = process.stdout.write;
  const originalErr = process.stderr.write;
  const delivered: unknown[] = [];
  const sink: typeof process.stdout.write = function (chunk, encodingOrCallback?, callback?) {
    delivered.push(chunk);
    if (typeof encodingOrCallback === "function") encodingOrCallback();
    else if (typeof callback === "function") callback();
    return false;
  };
  process.stdout.write = sink;
  process.stderr.write = sink;
  const events: OutputEvent[] = [];
  const restore = captureOutput((event) => events.push(event));
  try {
    const alien = Buffer.from("👽");
    let callbacks = 0;
    assert.equal(process.stdout.write(alien.subarray(0, 2), () => callbacks++), false);
    assert.equal(events.length, 0);
    assert.equal(process.stdout.write(alien.subarray(2)), false);
    process.stderr.write("warning", "utf8", () => callbacks++);
    process.stdout.write(new Uint8Array(Buffer.from("!")));
    assert.deepEqual(events, [
      { type: "stdout", text: "👽" },
      { type: "stderr", text: "warning" },
      { type: "stdout", text: "!" },
    ]);
    assert.equal(callbacks, 2);
    assert.equal(delivered.length, 4);
    writeStdout("");
    writeStderr("");
    assert.equal(events.length, 3);
    restore();
    assert.equal(process.stdout.write, sink);
    assert.equal(process.stderr.write, sink);
  } finally {
    restore();
    process.stdout.write = originalOut;
    process.stderr.write = originalErr;
  }
}));

test("asynchronous output is recorded after completion and between later evaluations", () => isolated(async () => {
  const trajectory = createTrajectory(createMemoryStore());
  let finish!: () => void;
  const later = new Promise<void>((resolve) => { finish = resolve; });
  const evaluator = createEvaluator({ finished: finish, waitForLater: later });
  const originalOut = process.stdout.write;
  const originalErr = process.stderr.write;
  process.stdout.write = () => true;
  process.stderr.write = () => true;
  const restore = captureOutput((event) => trajectory.append(event));
  try {
    trajectory.append({ type: "start" });
    const code = 'console.log("now"); console.error("warning"); setTimeout(() => { console.log("later"); finished(); }, 0);';
    const first = trajectory.append({ type: "eval", code });
    const result = await evaluator.evaluate(code);
    assert.deepEqual(result, { outcome: "return", text: "undefined" });
    trajectory.append({ type: "result", evalId: first.seq, outcome: "return", text: "undefined" });
    const beforeLater = trajectory.lastSeq();
    assert.deepEqual(trajectory.read().map((entry) => entry.event.type), ["start", "eval", "stdout", "stderr", "result"]);

    const second = trajectory.append({ type: "eval", code: "await waitForLater;" });
    await evaluator.evaluate("await waitForLater;");
    trajectory.append({ type: "result", evalId: second.seq, outcome: "return", text: "undefined" });
    assert.deepEqual(trajectory.read(beforeLater).map((entry) => entry.event), [
      { type: "eval", code: "await waitForLater;" },
      { type: "stdout", text: "later\n" },
      { type: "result", evalId: second.seq, outcome: "return", text: "undefined" },
    ]);
    assert.equal(trajectory.read().filter((entry) => entry.event.type === "stdout").length, 2);
  } finally {
    restore();
    process.stdout.write = originalOut;
    process.stderr.write = originalErr;
  }
}));

test("display prints input, source, results and calls, but not output that already reached the terminal", () => {
  const trajectory = createTrajectory(createMemoryStore());
  const shown: string[] = [];
  const show = (stream: string) => (text: string) => shown.push(`${stream}:${text.replace(/\x1b\[\d+m/g, "")}`);
  const stop = showTrajectory(trajectory, { out: show("out"), err: show("err"), note: show("note") });
  trajectory.append({ type: "start" });
  trajectory.append({ type: "stdin", text: "hi" });
  const evaluation = trajectory.append({ type: "eval", code: "return 1" });
  trajectory.append({ type: "stdout", text: "printed by code\n" });
  trajectory.append({ type: "result", evalId: evaluation.seq, outcome: "return", text: "1" });
  trajectory.append({ type: "result", evalId: evaluation.seq, outcome: "return", text: "undefined" });
  trajectory.append({ type: "result", evalId: evaluation.seq, outcome: "throw", text: "Error: boom" });
  trajectory.append({
    type: "generation", through: 7, model: "anthropic/test", latencyMs: 12, finishReason: "stop",
    usage: { uncachedInputTokens: 4, cacheReadTokens: 100, cacheWriteTokens: 6, outputTokens: 9, reasoningTokens: 0, costUSD: 0.0012 },
  });
  stop();
  trajectory.append({ type: "stdin", text: "unseen" });
  assert.deepEqual(shown, [
    "out:[you] hi\n", "out:\u26a1 return 1\n", "out:\u2192 1\n", "err:\u2718 Error: boom\n",
    "note:anthropic/test 12ms stop | 110 in, 100 cached, 9 out, $0.0012",
  ]);
});
