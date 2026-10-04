import assert from "node:assert/strict";
import { test } from "node:test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";

for (const [signal, code] of [["SIGINT", 130], ["SIGTERM", 143]] as const) {
  test(`${signal} stops the process during generation and leaves the database readable`, { timeout: 10_000 }, async (t) => {
    // A model API that never answers keeps the runtime waiting on generation.
    let requested!: () => void;
    const request = new Promise<void>((resolve) => { requested = resolve; });
    const server = createServer(() => requested());
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const dir = mkdtempSync(join(tmpdir(), "evalien-main-"));
    t.after(() => {
      server.closeAllConnections();
      server.close();
      rmSync(dir, { recursive: true, force: true });
    });

    const dbPath = join(dir, "agent.db");
    const child = spawn(process.execPath, ["--no-warnings", fileURLToPath(new URL("../src/main.mts", import.meta.url))], {
      env: {
        ANTHROPIC_API_KEY: "test-key",
        MODEL_BASE_URL: `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`,
        AGENT_DB_PATH: dbPath,
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stderr = "";
    child.stderr.on("data", (data) => { stderr += data; });
    await request;

    const exited = once(child, "exit");
    child.kill(signal);
    const [exitCode] = await exited;
    assert.equal(exitCode, code, stderr);
    assert.match(stderr, new RegExp(`${signal}: stopping`));

    const db = new DatabaseSync(dbPath, { readOnly: true });
    try {
      assert.equal(db.prepare("SELECT COUNT(*) AS n FROM trajectory").get()!.n, 1);
    } finally {
      db.close();
    }
  });
}

test("spending the budget stops the process with status 2 after recording the calls", { timeout: 10_000 }, async (t) => {
  // Every reply costs $0.06 at Sonnet 5.5 rates: 20k cache writes, 3k input, 400 output.
  let calls = 0;
  const server = createServer((_request, response) => {
    calls++;
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify({
      id: `msg_${calls}`, type: "message", role: "assistant", model: "claude-sonnet-5-5",
      content: [{ type: "text", text: JSON.stringify({ code: `return ${calls};` }) }],
      stop_reason: "end_turn", stop_sequence: null,
      usage: { input_tokens: 3000, output_tokens: 400, cache_creation_input_tokens: 20000 },
    }));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const dir = mkdtempSync(join(tmpdir(), "evalien-main-"));
  t.after(() => {
    server.closeAllConnections();
    server.close();
    rmSync(dir, { recursive: true, force: true });
  });

  const dbPath = join(dir, "agent.db");
  const child = spawn(process.execPath, ["--no-warnings", fileURLToPath(new URL("../src/main.mts", import.meta.url))], {
    env: {
      ANTHROPIC_API_KEY: "test-key",
      MODEL_BASE_URL: `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`,
      MODEL_BUDGET_USD: "0.1",
      AGENT_DB_PATH: dbPath,
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (data) => { stderr += data; });
  const [exitCode] = await once(child, "exit");
  assert.equal(exitCode, 2, stderr);
  assert.match(stderr, /Spent \$0\.1200 of this process's \$0\.10 budget\. Stopping\./);
  assert.equal(calls, 2);

  const db = new DatabaseSync(dbPath, { readOnly: true });
  try {
    const types = db.prepare("SELECT data FROM trajectory ORDER BY seq").all().map((row) => JSON.parse(String(row.data)).event.type);
    assert.deepEqual(types, ["start", "generation", "eval", "result", "generation", "eval", "result"]);
  } finally {
    db.close();
  }
});
