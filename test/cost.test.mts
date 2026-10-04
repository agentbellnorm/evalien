import assert from "node:assert/strict";
import { test } from "node:test";
import { GenerationError, type Generate, type Usage } from "../src/contracts.mts";
import { MAX_ENTRY_CHARS, MAX_PROMPT_CHARS, promptWindow, renderEntry } from "../src/harness/context.mts";
import { BudgetExceededError, withBudget } from "../src/inference/budget.mts";
import { costOf } from "../src/inference/pricing.mts";
import { createTrajectory } from "../src/trajectory/log.mts";
import { createMemoryStore } from "../src/trajectory/memory-store.mts";

const pricing = { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2.5 };
const tokens = { uncachedInputTokens: 1_000, cacheReadTokens: 10_000, cacheWriteTokens: 2_000, outputTokens: 500, reasoningTokens: 100 };
const usage = (costUSD: number): Usage => ({ ...tokens, costUSD });
const generation = (costUSD: number) => ({ model: "test", latencyMs: 1, finishReason: "stop", usage: usage(costUSD) });

test("cost prices each token class at its own rate", () => {
  // 1k × $2 + 10k × $0.20 + 2k × $2.50 + 500 × $10, per million.
  assert.equal(costOf(tokens, pricing), (2_000 + 2_000 + 5_000 + 5_000) / 1e6);
});

test("the budget counts completed and failed calls, and stops further generation", async () => {
  let calls = 0;
  const replies = [0.014, "fail", 0.014];
  const inner: Generate = async () => {
    const reply = replies[calls++];
    if (reply === "fail") throw new GenerationError("did not complete: length", { ...generation(0.004), finishReason: "length" });
    return { ...generation(reply as number), code: "void 0" };
  };
  const budget = withBudget(inner, 0.03);
  const input = { instructions: "i", blocks: ["b"] };
  assert.equal((await budget.generate(input)).code, "void 0");
  await assert.rejects(budget.generate(input), GenerationError);
  assert.equal((await budget.generate(input)).code, "void 0");
  // $0.032 spent: the call that crossed the budget completed, the next never starts.
  await assert.rejects(budget.generate(input), BudgetExceededError);
  assert.equal(calls, 3);
  assert.equal(budget.spent().toFixed(4), "0.0320");
});

test("oversized entries keep their head and tail and point at the full event", () => {
  const text = "a".repeat(MAX_ENTRY_CHARS) + "MIDDLE" + "z".repeat(MAX_ENTRY_CHARS);
  const rendered = renderEntry({ seq: 7, timestamp: "t", event: { type: "stdout", text } });
  assert.ok(rendered.length < MAX_ENTRY_CHARS + 500);
  assert.match(rendered, /^\[7 t stdout\]\n\| a/);
  assert.match(rendered, /z\n\n$/);
  assert.match(rendered, /8006 characters omitted; full text in trajectory seq 7/);
  assert.doesNotMatch(rendered, /MIDDLE/);
});

test("the prompt window stays aligned when small and bounded when output floods", () => {
  const trajectory = createTrajectory(createMemoryStore());
  for (let i = 0; i < 450; i++) trajectory.append({ type: "stdout", text: `line ${i}\n` });
  // Small entries: the usual 200-aligned start at seq 0.
  assert.equal(promptWindow(trajectory, 450).length, 450);

  for (let i = 0; i < 150; i++) trajectory.append({ type: "stdout", text: "x".repeat(MAX_ENTRY_CHARS) });
  const through = trajectory.lastSeq();
  const blocks = promptWindow(trajectory, through);
  assert.ok(blocks.reduce((sum, block) => sum + block.length, 0) <= MAX_PROMPT_CHARS);
  assert.equal(blocks.at(-1), renderEntry(trajectory.read(through - 1, through)[0]));
});
