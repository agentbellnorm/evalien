import assert from "node:assert/strict";
import { test } from "node:test";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import { captureOutput, writeStdout, writeStderr } from "../src/output.mts";
import { initTrajectory } from "../src/trajectory-store.mts";
import { createContext, evalCode } from "../src/eval.mts";
import type { OutputEvent } from "../src/trajectory.mts";

// The Node test runner itself writes a binary protocol to stdout. Exercise
// process-wide capture in a process that has no test reporter attached.
function isolated(check: () => void | Promise<void>): void {
  execFileSync(process.execPath, ["--no-warnings", "--input-type=module", "--eval", `
    import assert from 'node:assert/strict';
    import { DatabaseSync } from 'node:sqlite';
    import { captureOutput, writeStdout, writeStderr } from './src/output.mts';
    import { initTrajectory } from './src/trajectory-store.mts';
    import { createContext, evalCode } from './src/eval.mts';
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
  const db = new DatabaseSync(":memory:");
  const trajectory = initTrajectory(db);
  const ctx = createContext(db);
  const originalOut = process.stdout.write;
  const originalErr = process.stderr.write;
  process.stdout.write = () => true;
  process.stderr.write = () => true;
  const restore = captureOutput((event) => trajectory.append(event));
  try {
    trajectory.append({ type: "start" });
    let finish!: () => void;
    const later = new Promise<void>((resolve) => { finish = resolve; });
    ctx.finished = finish;
    const code = 'console.log("now"); console.error("warning"); setTimeout(() => { console.log("later"); finished(); }, 0);';
    const first = trajectory.append({ type: "eval", code });
    const result = await evalCode(ctx, code);
    assert.deepEqual(result, { result: undefined, error: null });
    trajectory.append({ type: "result", evalId: first.seq, outcome: "return", text: "undefined" });
    const beforeLater = trajectory.lastSeq();
    assert.deepEqual(trajectory.read().map((entry) => entry.event.type), ["start", "eval", "stdout", "stderr", "result"]);

    const second = trajectory.append({ type: "eval", code: "await waitForLater;" });
    ctx.waitForLater = later;
    await evalCode(ctx, "await waitForLater;");
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
    db.close();
  }
}));
