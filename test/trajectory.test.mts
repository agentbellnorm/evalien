import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import type { Entry, TrajectoryEvent } from "../src/contracts.mts";
import { createSqliteStore } from "../src/db/sqlite-store.mts";
import { renderEntry, renderTrajectory } from "../src/harness/context.mts";
import { deserializeEvent, serializeEvent } from "../src/trajectory/codec.mts";
import { createTrajectory } from "../src/trajectory/log.mts";
import { createMemoryStore } from "../src/trajectory/memory-store.mts";

const initTrajectory = (db: DatabaseSync) => createTrajectory(createSqliteStore(db, "trajectory"));

const timestamp = "2026-09-20T12:00:00.000Z";
const events: TrajectoryEvent[] = [
  { type: "start" },
  { type: "eval", code: 'const message = "👽";\nreturn message;' },
  { type: "stdin", text: '  hello\n[result]\n"quoted"\u0000  ' },
  { type: "stdout", text: "👽\n" },
  { type: "stderr", text: "warning\n" },
  { type: "result", evalId: 2, outcome: "return", text: "undefined" },
  { type: "result", evalId: 2, outcome: "throw", text: "TypeError: broken\n  at repl:1" },
  {
    type: "generation", through: 7, model: "anthropic/test", latencyMs: 2300, finishReason: "stop",
    usage: { uncachedInputTokens: 4, cacheReadTokens: 3150, cacheWriteTokens: 531, outputTokens: 213, reasoningTokens: 40, costUSD: 0.0041 },
  },
];
const entries: Entry[] = events.map((event, i) => ({ seq: i + 1, timestamp, event }));

test("every event round-trips without losing whitespace, Unicode, or outcome", () => {
  for (const event of events) {
    const encoded = serializeEvent(event);
    assert.deepEqual(deserializeEvent(encoded), event);
    assert.equal(serializeEvent(deserializeEvent(encoded)), encoded);
  }
});

test("serialization uses canonical field order and an explicit format version", () => {
  const event: TrajectoryEvent = { text: "3", outcome: "return", evalId: 2, type: "result" };
  assert.equal(serializeEvent(event), '{"version":1,"event":{"type":"result","evalId":2,"outcome":"return","text":"3"}}');
});

test("deserialization rejects corrupt, unknown, and incomplete records", () => {
  const invalid: unknown[] = [
    null, [], {}, { type: "start" },
    { version: 2, event: { type: "start" } },
    { version: 1, event: { type: "start" }, ignored: true },
    ...[
      null, [], {}, { type: "wake" }, { type: "start", extra: true },
      { type: "eval" }, { type: "eval", code: 42 },
      { type: "stdout", text: null },
      { type: "result", evalId: 2, outcome: "return" },
      { type: "result", evalId: 2, outcome: "timeout", text: "x" },
      { type: "generation", through: 1, model: "m", latencyMs: 1, finishReason: "stop" },
      ...[{ costUSD: -1 }, { costUSD: NaN }, { outputTokens: 1.5 }, { cacheReadTokens: "3" }].map((bad) => ({
        type: "generation", through: 1, model: "m", latencyMs: 1, finishReason: "stop",
        usage: { uncachedInputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, outputTokens: 0, reasoningTokens: 0, costUSD: 0, ...bad },
      })),
      ...[0, -1, 1.5, "2", Number.MAX_SAFE_INTEGER + 1].map((evalId) => ({
        type: "result", evalId, outcome: "return", text: "x",
      })),
    ].map((event) => ({ version: 1, event })),
  ];
  for (const value of invalid) assert.throws(() => deserializeEvent(JSON.stringify(value)), TypeError);
  assert.throws(() => deserializeEvent('{"version":1'), SyntaxError);
  assert.throws(() => serializeEvent({ type: "stdout", text: undefined } as unknown as TrajectoryEvent));
});

test("rendering shows event identity, completion status, and framed multiline content", () => {
  assert.equal(renderEntry(entries[5]),
    "[6 2026-09-20T12:00:00.000Z result eval=2 return]\n| undefined\n\n");
  assert.equal(renderEntry(entries[2]),
    '[3 2026-09-20T12:00:00.000Z stdin]\n|   hello\n| [result]\n| "quoted"\u0000  \n\n');
  assert.match(renderEntry(entries[0]), /Previous live bindings/);
  assert.match(renderEntry(entries[6]), /result eval=2 throw/);
  assert.equal(renderEntry(entries[7]), "[8 2026-09-20T12:00:00.000Z generation]\n" +
    "| anthropic/test through=7: 2.3s, stop, in 4 + 3150 cached + 531 written, out 213 (40 reasoning), $0.0041\n\n");
});

test("every appended suffix preserves the existing rendered prefix", () => {
  const whole = renderTrajectory(entries);
  for (let i = 0; i <= entries.length; i++) {
    const prefix = renderTrajectory(entries.slice(0, i));
    const suffix = renderTrajectory(entries.slice(i));
    assert.equal(prefix + suffix, whole);
  }
});

test("SQLite persists a detached, ordered trajectory and reads fixed prefixes", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "evalien-trajectory-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const path = join(dir, "state.db");
  let db = new DatabaseSync(path);
  let trajectory = initTrajectory(db);
  assert.equal(trajectory.lastSeq(), 0);
  assert.deepEqual(trajectory.read(), []);

  trajectory.append({ type: "start" });
  const evaluation = trajectory.append({ type: "eval", code: "return 3" });
  const mutable: TrajectoryEvent = { type: "stdout", text: "original\n" };
  const appended = trajectory.append(mutable);
  mutable.text = "changed";
  if (appended.event.type === "stdout") appended.event.text = "also changed";
  trajectory.append({ type: "result", evalId: evaluation.seq, outcome: "return", text: "3" });

  const through = trajectory.lastSeq();
  const prefix = trajectory.read(0, through);
  const rendered = renderTrajectory(prefix);
  assert.deepEqual(prefix.map((entry) => entry.seq), [1, 2, 3, 4]);
  assert.equal(prefix[2].event.type === "stdout" && prefix[2].event.text, "original\n");
  for (const entry of prefix) assert.equal(new Date(entry.timestamp).toISOString(), entry.timestamp);
  db.close();

  db = new DatabaseSync(path);
  t.after(() => db.close());
  trajectory = initTrajectory(db);
  assert.deepEqual(trajectory.read(), prefix);
  assert.equal(await trajectory.waitAfter(0), through);
  const restart = trajectory.append({ type: "start" });
  assert.equal(restart.seq, 5);
  assert.equal(renderTrajectory(trajectory.read(0, through)), rendered);
  assert.deepEqual(trajectory.read(through), [restart]);
  assert.deepEqual(trajectory.read(2, 4), prefix.slice(2));
  assert.deepEqual(trajectory.read(4, 2), []);
});

test("invalid events and dangling results cannot be appended", async (t) => {
  const db = new DatabaseSync(":memory:");
  t.after(() => db.close());
  const trajectory = initTrajectory(db);
  const start = trajectory.append({ type: "start" });
  for (const evalId of [start.seq, 999]) {
    assert.throws(() => trajectory.append({ type: "result", evalId, outcome: "return", text: "x" }), /missing evaluation/);
  }
  assert.throws(() => trajectory.append({ type: "stdout", text: 3 } as unknown as TrajectoryEvent));
  assert.equal(trajectory.lastSeq(), 1);
  for (const cursor of [-1, 1.5, NaN, Infinity]) {
    assert.throws(() => trajectory.read(cursor));
    await assert.rejects(trajectory.waitAfter(cursor));
  }
});

test("waiting consumers see persisted bursts and catch up on arrivals between waits", async (t) => {
  const db = new DatabaseSync(":memory:");
  t.after(() => db.close());
  const trajectory = initTrajectory(db);
  let notified = false;
  const waiting = trajectory.waitAfter(0).then((seq) => { notified = true; return seq; });
  await Promise.resolve();
  assert.equal(notified, false);

  trajectory.append({ type: "start" });
  trajectory.append({ type: "stdout", text: "one" });
  const through = await waiting;
  assert.equal(through, 2);
  assert.deepEqual(trajectory.read(0, through).map(({ event }) => event.type), ["start", "stdout"]);

  trajectory.append({ type: "stdout", text: "two" });
  trajectory.append({ type: "stderr", text: "three" });
  assert.equal(await trajectory.waitAfter(through), 4);
  assert.equal(trajectory.read(0, through).length, 2);
});

test("consumers have independent cursors and only wake beyond their sequence", async (t) => {
  const db = new DatabaseSync(":memory:");
  t.after(() => db.close());
  const trajectory = initTrajectory(db);
  const first = trajectory.waitAfter(0);
  const second = trajectory.waitAfter(0);
  let futureNotified = false;
  const future = trajectory.waitAfter(1).then((seq) => { futureNotified = true; return seq; });

  trajectory.append({ type: "start" });
  assert.deepEqual(await Promise.all([first, second]), [1, 1]);
  assert.equal(futureNotified, false);
  trajectory.append({ type: "stdin", text: "hello" });
  assert.equal(await future, 2);
  assert.equal(await trajectory.waitAfter(0), 2);
  assert.equal(trajectory.read().length, 2);
});

test("rejected appends do not notify consumers", async (t) => {
  const db = new DatabaseSync(":memory:");
  t.after(() => db.close());
  const trajectory = initTrajectory(db);
  let notified = false;
  const waiting = trajectory.waitAfter(0).then((seq) => { notified = true; return seq; });
  assert.throws(() => trajectory.append({ type: "result", evalId: 99, outcome: "return", text: "undefined" }));
  await Promise.resolve();
  assert.equal(notified, false);
  trajectory.append({ type: "start" });
  assert.equal(await waiting, 1);
});

test("corrupt persisted events fail visibly instead of silently changing history", (t) => {
  const db = new DatabaseSync(":memory:");
  t.after(() => db.close());
  const trajectory = initTrajectory(db);
  db.prepare("INSERT INTO trajectory (timestamp, data) VALUES (?, ?)").run(timestamp, '{"version":1,"event":{"type":"unknown"}}');
  assert.throws(() => trajectory.read(), /Unknown trajectory event/);
});

test("the memory store behaves like SQLite behind the trajectory", async () => {
  const trajectory = createTrajectory(createMemoryStore());
  const seen: Entry[] = [];
  const stop = trajectory.subscribe((entry) => seen.push(entry));
  const waiting = trajectory.waitAfter(0);
  const evaluation = trajectory.append({ type: "eval", code: "return 1" });
  trajectory.append({ type: "result", evalId: evaluation.seq, outcome: "return", text: "1" });
  assert.throws(() => trajectory.append({ type: "result", evalId: 99, outcome: "return", text: "x" }), /missing evaluation/);
  assert.equal(await waiting, 2);
  assert.deepEqual(trajectory.read(1).map(({ event }) => event.type), ["result"]);
  assert.deepEqual(trajectory.read(2, 1), []);
  stop();
  trajectory.append({ type: "start" });
  // Listeners see each accepted append once, in order, and stop when unsubscribed.
  assert.deepEqual(seen.map(({ seq, event }) => [seq, event.type]), [[1, "eval"], [2, "result"]]);
  seen[0].event = { type: "start" };
  assert.equal(trajectory.read(0, 1)[0].event.type, "eval");
});
