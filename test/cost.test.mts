import assert from "node:assert/strict";
import { test } from "node:test";
import { DatabaseSync } from "node:sqlite";
import { BudgetExceededError, costOf, createMeter, type GenerationReport } from "../src/meter.mts";
import { MAX_PROMPT_CHARS, promptWindow } from "../src/runtime.mts";
import { MAX_ENTRY_CHARS, renderEntry } from "../src/trajectory.mts";
import { initTrajectory } from "../src/trajectory-store.mts";

const pricing = { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2.5 };
const report = (overrides: Partial<GenerationReport> = {}): GenerationReport => ({
  latencyMs: 100, finishReason: "stop", uncachedInputTokens: 1_000, cacheReadTokens: 10_000,
  cacheWriteTokens: 2_000, outputTokens: 500, reasoningTokens: 100, ...overrides,
});

test("cost prices each token class at its own rate", () => {
  // 1k × $2 + 10k × $0.20 + 2k × $2.50 + 500 × $10, per million.
  assert.equal(costOf(report(), pricing), (2_000 + 2_000 + 5_000 + 5_000) / 1e6);
});

test("every billed response is logged, and the budget stops further generation", async () => {
  const db = new DatabaseSync(":memory:");
  const meter = createMeter(db, { model: "anthropic/test", pricing, budgetUSD: 0.02, position: () => 41 });
  let calls = 0;
  const generate = meter.guard(async () => {
    calls++;
    meter.record(report());
    return "void 0";
  });
  const input = { instructions: "i", blocks: ["b"] };
  assert.equal(await generate(input), "void 0");
  assert.equal(await generate(input), "void 0");
  // $0.028 spent: the call that crossed the budget completes, the next never starts.
  await assert.rejects(generate(input), BudgetExceededError);
  assert.equal(calls, 2);

  const rows = db.prepare("SELECT * FROM generations ORDER BY id").all();
  assert.equal(rows.length, 2);
  assert.equal(rows[0].model, "anthropic/test");
  assert.equal(rows[0].through_seq, 41);
  assert.equal(rows[0].cache_read_tokens, 10_000);
  assert.equal(rows[0].cost_usd, 0.014);

  // Evaluated code shares the database. Clearing the log doesn't reset spend.
  db.exec("DELETE FROM generations");
  await assert.rejects(generate(input), BudgetExceededError);
  assert.equal(meter.spent(), 0.028);
});

test("oversized entries keep their head and tail and point at the full event", () => {
  const text = "a".repeat(MAX_ENTRY_CHARS) + "MIDDLE" + "z".repeat(MAX_ENTRY_CHARS);
  const rendered = renderEntry({ seq: 7, timestamp: "t", event: { type: "stdout", text } });
  assert.ok(rendered.length < MAX_ENTRY_CHARS + 500);
  assert.match(rendered, /^\[7 t stdout\]\n\| a/);
  assert.match(rendered, /z\n\n$/);
  assert.match(rendered, /8006 characters omitted; full event: SELECT event FROM trajectory WHERE seq = 7/);
  assert.doesNotMatch(rendered, /MIDDLE/);
});

test("the prompt window stays aligned when small and bounded when output floods", () => {
  const trajectory = initTrajectory(new DatabaseSync(":memory:"));
  for (let i = 0; i < 450; i++) trajectory.append({ type: "stdout", text: `line ${i}\n` });
  // Small entries: the usual 200-aligned start at seq 0.
  assert.equal(promptWindow(trajectory, 450).length, 450);

  for (let i = 0; i < 150; i++) trajectory.append({ type: "stdout", text: "x".repeat(MAX_ENTRY_CHARS) });
  const through = trajectory.lastSeq();
  const blocks = promptWindow(trajectory, through);
  assert.ok(blocks.reduce((sum, block) => sum + block.length, 0) <= MAX_PROMPT_CHARS);
  assert.equal(blocks.at(-1), renderEntry(trajectory.read(through - 1, through)[0]));
});
