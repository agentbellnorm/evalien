import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import {
  GenerationError, type Evaluation, type Generation, type Generate, type GenerateInput, type InputSource, type OutputSource,
} from "../src/contracts.mts";
import { renderEntry } from "../src/harness/context.mts";
import { runRuntime } from "../src/harness/runtime.mts";
import { createTrajectory } from "../src/trajectory/log.mts";
import { createMemoryStore } from "../src/db/memory-store.mts";

const usage = { uncachedInputTokens: 1, cacheReadTokens: 2, cacheWriteTokens: 3, outputTokens: 4, reasoningTokens: 0, costUSD: 0.001 };
const generation: Generation = { model: "test/model", latencyMs: 5, finishReason: "stop", usage };

async function until(condition: () => boolean): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (!condition()) {
    if (Date.now() > deadline) assert.fail("Timed out waiting for the harness");
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
}

/**
 * The harness with every dependency in memory. Model calls wait for reply();
 * evaluations settle immediately with undefined unless the code is "hold",
 * which waits for release().
 */
function start(t: TestContext, idleDelays: number[] = []) {
  const log = createTrajectory(createMemoryStore());
  const requests: GenerateInput[] = [];
  const pending: { resolve: (code: string) => void; reject: (err: unknown) => void }[] = [];
  const generate: Generate = (input) => {
    requests.push(input);
    return new Promise((resolve, reject) => {
      pending.push({ resolve: (code) => resolve({ ...generation, code }), reject });
    });
  };
  const held: ((evaluation: Evaluation) => void)[] = [];
  const evaluator = {
    evaluate: (code: string) => code === "hold"
      ? new Promise<Evaluation>((resolve) => held.push(resolve))
      : Promise.resolve<Evaluation>({ outcome: "return", text: `ran ${code}` }),
  };
  let type: ((line: string) => void) | undefined;
  const input: InputSource = (onLine) => { type = onLine; return () => { type = undefined; }; };
  let print: Parameters<OutputSource>[0] | undefined;
  const output: OutputSource = (emit) => { print = emit; return () => { print = undefined; }; };

  const stopped = new Error("test finished");
  const done = runRuntime({ log, generate, evaluator, input, output, instructions: "test instructions", idleDelays });
  t.after(async () => {
    for (const call of pending) call.reject(stopped);
    await done.catch(() => {});
  });

  const all = () => log.read();
  return {
    log, requests, done, all,
    types: () => all().map(({ event }) => event.type),
    reply(code: string) {
      const call = pending.shift();
      assert.ok(call, "no model call is waiting");
      call.resolve(code);
    },
    fail(err: unknown) { pending.shift()!.reject(err); },
    release(text = "released") { held.shift()!({ outcome: "return", text }); },
    type: (line: string) => type!(line),
    print: (text: string) => print!({ type: "stdout", text }),
    /** What a prompt through this seq contains. */
    through: (seq: number) => all().filter((e) => e.seq <= seq).map(renderEntry),
  };
}

test("every result wakes the model, with the trajectory through that point", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  assert.deepEqual(h.requests[0], { instructions: "test instructions", blocks: h.through(1) });

  h.reply("first");
  await until(() => h.requests.length === 2);
  assert.deepEqual(h.types(), ["start", "generation", "eval", "result"]);
  const [, call, evaluation, result] = h.all();
  assert.deepEqual(call.event, { type: "generation", through: 1, ...generation });
  assert.deepEqual(evaluation.event, { type: "eval", code: "first" });
  assert.deepEqual(result.event, { type: "result", evalId: evaluation.seq, outcome: "return", text: "ran first" });
  assert.deepEqual(h.requests[1].blocks, h.through(4));
});

test("input during a model call is kept for the next one", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  h.type("first");
  h.type("second");
  h.reply("work");
  await until(() => h.requests.length === 2);
  // The result settled before the next decision, so one call sees both.
  assert.deepEqual(h.types(), ["start", "stdin", "stdin", "generation", "eval", "result"]);
  assert.deepEqual(h.requests[1].blocks, h.through(6));
});

test("human input reaches the model while an evaluation is pending, and results settle in any order", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  h.reply("hold");
  await until(() => h.types().includes("eval"));
  // Output from a pending evaluation doesn't wake the model by itself.
  h.print("progress\n");
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(h.requests.length, 1);

  h.type("hello?");
  await until(() => h.requests.length === 2);
  assert.deepEqual(h.requests[1].blocks, h.through(h.log.lastSeq()));
  h.reply("second");
  await until(() => h.requests.length === 3);
  h.release("'resumed'");
  await until(() => h.types().filter((type) => type === "result").length === 2);
  const results = h.all().filter(({ event }) => event.type === "result");
  const evals = h.all().filter(({ event }) => event.type === "eval");
  assert.deepEqual(results.map(({ event }) => event.type === "result" && event.evalId), [evals[1].seq, evals[0].seq]);
  // The third call was already running when the first evaluation settled.
  h.reply("");
  await until(() => h.requests.length === 4);
  assert.deepEqual(h.requests[3].blocks, h.through(h.log.lastSeq()));
});

test("empty code ends the turn; the next input or idle output starts another", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  h.reply("  \n");
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(h.requests.length, 1);
  assert.deepEqual(h.types(), ["start", "generation"]);

  h.type("wake up");
  await until(() => h.requests.length === 2);
  h.reply("");
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(h.requests.length, 2);
  // With nothing pending, output from background code wakes the model.
  h.print("alert\n");
  await until(() => h.requests.length === 3);
  assert.deepEqual(h.types(), ["start", "generation", "stdin", "generation", "stdout"]);
});

test("quiet calls are spaced out, and human input ends the wait", async (t) => {
  const h = start(t, [0, 300]);
  await until(() => h.requests.length === 1);
  const at = [Date.now()];
  h.reply("one");
  await until(() => h.requests.length === 2);
  at.push(Date.now());
  h.reply("two");
  await until(() => h.requests.length === 3);
  at.push(Date.now());
  assert.ok(at[1] - at[0] < 200, `first quiet call took ${at[1] - at[0]}ms`);
  assert.ok(at[2] - at[1] >= 250, `second quiet call came after ${at[2] - at[1]}ms`);

  h.reply("three");
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(h.requests.length, 3);
  const sent = Date.now();
  h.type("hurry");
  await until(() => h.requests.length === 4);
  assert.ok(Date.now() - sent < 200, `input waited ${Date.now() - sent}ms`);
  assert.equal(h.all().at(-1)!.event.type, "stdin");
  assert.deepEqual(h.requests[3].blocks, h.through(h.log.lastSeq()));
});

test("a failed call is recorded with its usage, then stops the harness", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  const failure = new GenerationError("Generation did not complete: length", { ...generation, finishReason: "length" });
  h.fail(failure);
  await assert.rejects(h.done, failure);
  assert.deepEqual(h.all().at(-1)!.event, { type: "generation", through: 1, ...generation, finishReason: "length" });
});

test("other errors stop the harness without a recorded call", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  h.fail(new Error("budget spent"));
  await assert.rejects(h.done, /budget spent/);
  assert.deepEqual(h.types(), ["start"]);
});

test("the harness stops listening when it stops", async (t) => {
  const h = start(t);
  await until(() => h.requests.length === 1);
  h.fail(new Error("stop"));
  await h.done.catch(() => {});
  assert.throws(() => h.type("late"), TypeError);
  assert.throws(() => h.print("late"), TypeError);
  assert.equal(h.all().length, 1);
});
