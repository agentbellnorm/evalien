import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { fork } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import type { GenerateInput } from "../src/generation.mts";
import { deserializeEvent, renderEntry, type Entry } from "../src/trajectory.mts";

async function until(condition: () => boolean): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (!condition()) {
    if (Date.now() > deadline) assert.fail("Timed out waiting for the runtime");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

async function startRuntime(t: TestContext, idleDelays?: number[]) {
  const dir = mkdtempSync(join(tmpdir(), "evalien-runtime-"));
  const dbPath = join(dir, "state.db");
  const requests: GenerateInput[] = [];
  const child = fork(fileURLToPath(new URL("fixtures/runtime.mts", import.meta.url)), [dbPath, ...(idleDelays ? [JSON.stringify(idleDelays)] : [])], {
    execArgv: ["--experimental-strip-types"],
    env: { NODE_NO_WARNINGS: "1" },
    stdio: ["pipe", "pipe", "pipe", "ipc"],
  });
  child.on("message", (message: { id: number; input: GenerateInput }) => {
    assert.equal(message.id, requests.length);
    requests.push(message.input);
  });
  let terminal = "";
  child.stdout!.on("data", (data) => { terminal += data; });
  child.stderr!.on("data", (data) => { terminal += data; });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, "exit");
      child.kill();
      await exited;
    }
    rmSync(dir, { recursive: true, force: true });
  });
  await until(() => requests.length > 0 || child.exitCode !== null);
  assert.ok(requests.length > 0, terminal);
  return {
    requests,
    child,
    respond(id: number, code: string) {
      child.send({ type: "generated", id, code });
    },
    read() {
      const db = new DatabaseSync(dbPath, { readOnly: true });
      db.exec("PRAGMA busy_timeout = 1000");
      try {
        return db.prepare("SELECT * FROM trajectory ORDER BY seq").all().map((row): Entry => ({
          seq: Number(row.seq), timestamp: String(row.timestamp), event: deserializeEvent(String(row.event)),
        }));
      }
      finally { db.close(); }
    },
  };
}

test("every completion, including undefined, continues with a stable chronological prompt", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t);
  const codes = [
    'console.log("sync"); console.error("warning"); process.stdout.write("raw\\n"); return 7n;',
    "void 0",
    'return "undefined";',
    "return;",
  ];
  for (const [index, code] of codes.entries()) {
    runtime.respond(index, code);
    await until(() => runtime.requests.length === index + 2);
  }
  const entries = runtime.read();
  assert.deepEqual(entries.map((entry) => entry.event.type), [
    "start", "eval", "stdout", "stderr", "stdout", "result",
    "eval", "result", "eval", "result", "eval", "result",
  ]);
  assert.deepEqual(entries[5].event, { type: "result", evalId: 2, outcome: "return", text: "7n" });
  assert.deepEqual(entries[7].event, { type: "result", evalId: 7, outcome: "return", text: "undefined" });
  assert.deepEqual(entries[9].event, { type: "result", evalId: 9, outcome: "return", text: "'undefined'" });
  assert.deepEqual(entries[11].event, { type: "result", evalId: 11, outcome: "return", text: "undefined" });

  for (const [index, through] of [1, 6, 8, 10, 12].entries()) {
    const request = runtime.requests[index];
    assert.deepEqual(Object.keys(request), ["instructions", "blocks"]);
    assert.equal(request.instructions, "Runtime test instructions");
    assert.deepEqual(request.blocks, entries.slice(0, through).map(renderEntry));
  }
});

test("input received during inference is retained for the next request", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t);
  runtime.child.send({ type: "input", text: "  first  \nsecond\n" });
  await until(() => runtime.read().filter((entry) => entry.event.type === "stdin").length === 2);
  assert.equal(runtime.requests.length, 1);
  runtime.respond(0, "void 0");
  await until(() => runtime.requests.length === 2);
  const entries = runtime.read();
  assert.deepEqual(entries.map((entry) => entry.event.type), [
    "start", "stdin", "stdin", "eval", "result",
  ]);
  assert.deepEqual(entries[1].event, { type: "stdin", text: "  first  " });
  assert.deepEqual(runtime.requests[0].blocks, entries.slice(0, 1).map(renderEntry));
  assert.deepEqual(runtime.requests[1].blocks, entries.map(renderEntry));
});

test("syntax errors and rejected evaluations each produce a result and continue inference", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t);
  const codes = ["const =", 'throw new Error("boom")', 'await Promise.reject(new Error("async failure"))'];
  for (const [index, code] of codes.entries()) {
    runtime.respond(index, code);
    await until(() => runtime.requests.length === index + 2);
  }
  const events = runtime.read().map((entry) => entry.event);
  assert.deepEqual(events.map((event) => event.type), [
    "start", "eval", "result", "eval", "result", "eval", "result",
  ]);
  const results = events.filter((event) => event.type === "result");
  assert.deepEqual(results.map((event) => event.outcome), ["throw", "throw", "throw"]);
  assert.match(results[0].text, /^SyntaxError:/);
  assert.equal(results[1].text, "Error: boom");
  assert.equal(results[2].text, "Error: async failure");
});

test("human input reaches the model while an earlier evaluation is still running", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t);
  runtime.respond(0, `
    setTimeout(() => console.log("later"), 20);
    console.log("waiting");
    await new Promise(resolve => process.stdin.once("data", resolve));
    return "resumed";
  `);
  // Output from a pending evaluation doesn't request inference by itself.
  await until(() => runtime.read().some(({ event }) => event.type === "stdout" && event.text === "later\n"));
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal(runtime.requests.length, 1);

  runtime.child.send({ type: "input", text: "hello while waiting\n" });
  await until(() => runtime.requests.length === 2);
  assert.deepEqual(runtime.read().map(({ event }) => event.type), ["start", "eval", "stdout", "stdout", "stdin"]);
  assert.deepEqual(runtime.requests[1].blocks, runtime.read().map(renderEntry));

  // A second evaluation runs and settles while the first is still waiting.
  runtime.respond(1, 'return "second";');
  await until(() => runtime.requests.length === 3);
  // The first evaluation settles during the third model call; that call's
  // prompt doesn't include it, the next one does.
  runtime.child.stdin!.write("resume\n");
  await until(() => runtime.read().length === 8);
  assert.deepEqual(runtime.requests[2].blocks, runtime.read().slice(0, 7).map(renderEntry));
  runtime.respond(2, "void 0");
  await until(() => runtime.requests.length === 4);
  const entries = runtime.read();
  assert.deepEqual(entries.map(({ event }) => event.type), [
    "start", "eval", "stdout", "stdout", "stdin", "eval", "result", "result", "eval", "result",
  ]);
  assert.deepEqual(entries[6].event, { type: "result", evalId: 6, outcome: "return", text: "'second'" });
  assert.deepEqual(entries[7].event, { type: "result", evalId: 2, outcome: "return", text: "'resumed'" });
  assert.deepEqual(runtime.requests[3].blocks, entries.map(renderEntry));
});

test("quiet model calls are spaced out, and human input ends the wait", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t, [0, 400]);
  const at: number[] = [Date.now()];
  runtime.respond(0, "void 0");
  await until(() => runtime.requests.length === 2);
  at.push(Date.now());
  runtime.respond(1, "void 0");
  await until(() => runtime.requests.length === 3);
  at.push(Date.now());
  // First quiet call immediate, second at least 400ms after the previous call.
  assert.ok(at[1] - at[0] < 300, `first quiet call took ${at[1] - at[0]}ms`);
  assert.ok(at[2] - at[1] >= 350, `second quiet call came after ${at[2] - at[1]}ms`);

  runtime.respond(2, "void 0");
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal(runtime.requests.length, 3);
  const sent = Date.now();
  runtime.child.send({ type: "input", text: "hurry\n" });
  await until(() => runtime.requests.length === 4);
  assert.ok(Date.now() - sent < 300, `input waited ${Date.now() - sent}ms`);
  assert.equal(runtime.requests[3].blocks.at(-1), renderEntry(runtime.read().at(-1)!));
  assert.equal(runtime.read().at(-1)!.event.type, "stdin");
});

test("empty code ends the turn until something happens", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t);
  runtime.respond(0, "  \n");
  await new Promise((resolve) => setTimeout(resolve, 100));
  // Nothing evaluated, nothing recorded, no further call.
  assert.equal(runtime.requests.length, 1);
  assert.deepEqual(runtime.read().map(({ event }) => event.type), ["start"]);

  runtime.child.send({ type: "input", text: "wake up\n" });
  await until(() => runtime.requests.length === 2);
  assert.deepEqual(runtime.requests[1].blocks, runtime.read().map(renderEntry));

  // An idle agent with a background watcher is woken by its output.
  runtime.respond(1, 'process.stdin.once("data", () => console.log("alert"));');
  await until(() => runtime.requests.length === 3);
  runtime.respond(2, "");
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(runtime.requests.length, 3);
  runtime.child.stdin!.write("trigger\n");
  await until(() => runtime.requests.length === 4);
  assert.deepEqual(runtime.read().map(({ event }) => event.type), ["start", "stdin", "eval", "result", "stdout"]);
});

test("asynchronous output arriving during generation is included in the following prompt", { timeout: 10_000 }, async (t) => {
  const runtime = await startRuntime(t);
  runtime.respond(0, `
    process.stdin.once("data", () => {
      console.log("later output");
      console.error("later warning");
    });
  `);
  await until(() => runtime.requests.length === 2);
  const prefix = runtime.read();
  runtime.child.stdin!.write("produce output\n");
  await until(() => runtime.read().some(({ event }) => event.type === "stderr"));
  assert.equal(runtime.requests.length, 2);
  assert.deepEqual(runtime.requests[1].blocks, prefix.map(renderEntry));

  runtime.respond(1, "void 0");
  await until(() => runtime.requests.length === 3);
  const entries = runtime.read();
  assert.deepEqual(entries.map(({ event }) => event.type), ["start", "eval", "result", "stdout", "stderr", "eval", "result"]);
  assert.deepEqual(runtime.requests[2].blocks, entries.map(renderEntry));
});
