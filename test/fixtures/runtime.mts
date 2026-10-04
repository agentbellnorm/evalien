import { DatabaseSync } from "node:sqlite";
import { PassThrough } from "node:stream";
import { runRuntime } from "../../src/runtime.mts";
import type { Generate } from "../../src/generation.mts";

// Exercise the runtime in its own process, with an injected function and no SDK.
const db = new DatabaseSync(process.argv[2]);
const input = new PassThrough();
let nextId = 0;
const pending = new Map<number, (code: string) => void>();
const generate: Generate = (request) => new Promise((resolve) => {
  const id = nextId++;
  pending.set(id, resolve);
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
  await runRuntime({ db, input, generate, instructions: "Runtime test instructions", idleDelays });
} finally {
  db.close();
}
