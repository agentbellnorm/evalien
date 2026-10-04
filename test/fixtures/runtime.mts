import { DatabaseSync } from "node:sqlite";
import { PassThrough } from "node:stream";
import { createSqliteStore } from "../../src/db/sqlite-store.mts";
import { runRuntime } from "../../src/harness/runtime.mts";
import { createTrajectory } from "../../src/trajectory/log.mts";
import type { Generate } from "../../src/contracts.mts";

// Exercise the runtime in its own process, with an injected function and no SDK.
const db = new DatabaseSync(process.argv[2]);
const input = new PassThrough();
let nextId = 0;
const pending = new Map<number, (code: string) => void>();
const usage = { uncachedInputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, outputTokens: 0, reasoningTokens: 0, costUSD: 0 };
const generate: Generate = (request) => new Promise((resolve) => {
  const id = nextId++;
  pending.set(id, (code) => resolve({ code, model: "test/fixture", latencyMs: 0, finishReason: "stop", usage }));
  process.send!({ id, input: request });
});

type Message =
  | { type: "input"; text: string }
  | { type: "generated"; id: number; code: string };
process.on("message", (message: Message) => {
  if (message.type === "input") input.write(message.text);
  else {
    const request = pending.get(message.id)!;
    pending.delete(message.id);
    request(message.code);
  }
});

try {
  const idleDelays = process.argv[3] ? JSON.parse(process.argv[3]) as number[] : [];
  await runRuntime({ log: createTrajectory(createSqliteStore(db, "trajectory")), db, input, generate, instructions: "Runtime test instructions", idleDelays });
} finally {
  db.close();
}
