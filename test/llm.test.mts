import assert from "node:assert/strict";
import { test } from "node:test";
import { createGenerate, readModelConfig, type ModelConfig } from "../src/llm.mts";
import type { GenerationReport } from "../src/meter.mts";

type Provider = ModelConfig["provider"];
type Part = { text: string; cache_control?: { type: string } };
interface WireRequest {
  model?: string;
  system?: Part[];
  messages?: { content: Part[] }[];
  input?: { role: string; content: string | Part[] }[];
  systemInstruction?: { parts: Part[] };
  contents?: { parts: Part[] }[];
  max_tokens?: number;
  output_config?: { format: { type: string; schema: { required: string[] } } };
  text?: { format: { type: string; schema: { required: string[] } } };
  max_output_tokens?: number;
  generationConfig?: { maxOutputTokens: number; responseJsonSchema?: { required: string[] } };
}

function response(provider: Provider, code: string, truncated = false): object {
  // Structured replies carry the source in a JSON object; a truncated one is cut mid-object.
  const reply = JSON.stringify({ code });
  const text = truncated ? reply.slice(0, -2) : reply;
  switch (provider) {
    case "anthropic": return {
      id: "msg_test", type: "message", role: "assistant", model: "test-model",
      content: [{ type: "text", text }],
      stop_reason: truncated ? "max_tokens" : "end_turn", stop_sequence: null,
      usage: { input_tokens: 20, output_tokens: 5, cache_read_input_tokens: 10 },
    };
    case "openai": return {
      id: "resp_test", object: "response", model: "test-model", created_at: 0,
      status: truncated ? "incomplete" : "completed",
      output: [{
        type: "message", id: "msg_test", role: "assistant", status: "completed",
        content: [{ type: "output_text", text, annotations: [] }],
      }],
      incomplete_details: truncated ? { reason: "max_output_tokens" } : null,
      usage: { input_tokens: 20, output_tokens: 5, total_tokens: 25, input_tokens_details: { cached_tokens: 10 } },
    };
    case "google": return {
      candidates: [{
        content: { role: "model", parts: [{ text }] },
        finishReason: truncated ? "MAX_TOKENS" : "STOP", index: 0,
      }],
      usageMetadata: { promptTokenCount: 20, candidatesTokenCount: 5, totalTokenCount: 25, cachedContentTokenCount: 10 },
      modelVersion: "test-model",
    };
  }
}

function transport(provider: Provider, replies: { code: string; truncated?: boolean }[] = [{ code: "return 42;" }]) {
  const requests: { url: string; headers: Headers; body: WireRequest }[] = [];
  const fetch: typeof globalThis.fetch = async (url, options) => {
    const reply = replies[Math.min(requests.length, replies.length - 1)];
    requests.push({ url: String(url), headers: new Headers(options?.headers), body: JSON.parse(String(options?.body)) });
    return Response.json(response(provider, reply.code, reply.truncated));
  };
  return { requests, fetch };
}

const keys = {
  anthropic: "ANTHROPIC_API_KEY",
  openai: "OPENAI_API_KEY",
  google: "GOOGLE_GENERATIVE_AI_API_KEY",
};

for (const provider of ["anthropic", "openai", "google"] as const) {
  test(`${provider}: configured model, credentials and ordered blocks reach the real SDK adapter`, async () => {
    const env: Record<string, string> = {
      MODEL: `${provider}/test-model`,
      [keys[provider]]: "test-key",
      MODEL_BASE_URL: "https://test.invalid/v1",
      MODEL_MAX_OUTPUT_TOKENS: "8192",
      MODEL_PRICING: "1,2",
    };
    const { requests, fetch } = transport(provider);
    const generate = createGenerate(readModelConfig(env), fetch);
    for (const name of Object.keys(env)) delete env[name];
    const blocks = ["first\n", "second 👽\n", "third\n"];
    const frozen = Object.freeze([...blocks]);
    assert.equal(await generate({ instructions: "instructions", blocks: frozen }), "return 42;");
    assert.deepEqual(frozen, blocks);
    assert.equal(requests.length, 1);
    const { url, headers, body } = requests[0];

    if (provider === "anthropic") {
      assert.equal(url, "https://test.invalid/v1/messages");
      assert.equal(headers.get("x-api-key"), "test-key");
      assert.equal(body.model, "test-model");
      assert.deepEqual(body.system?.map((part) => part.text), ["instructions"]);
      assert.deepEqual(body.messages?.[0].content.map((part) => part.text), blocks);
      assert.equal(body.max_tokens, 8192);
    } else if (provider === "openai") {
      assert.equal(url, "https://test.invalid/v1/responses");
      assert.equal(headers.get("authorization"), "Bearer test-key");
      assert.equal(body.model, "test-model");
      assert.equal(body.input?.[0].content, "instructions");
      const content = body.input?.[1].content;
      assert.ok(Array.isArray(content));
      assert.deepEqual(content.map((part) => part.text), blocks);
      assert.equal(body.max_output_tokens, 8192);
    } else {
      assert.equal(url, "https://test.invalid/v1/models/test-model:generateContent");
      assert.equal(headers.get("x-goog-api-key"), "test-key");
      assert.deepEqual(body.systemInstruction?.parts.map((part) => part.text), ["instructions"]);
      assert.deepEqual(body.contents?.[0].parts.map((part) => part.text), blocks);
      assert.equal(body.generationConfig?.maxOutputTokens, 8192);
    }
    if (provider !== "anthropic") assert.equal(JSON.stringify(body).includes("cache_control"), false);
    // Each provider constrains the reply to {code} natively, not with a forced tool.
    const schema = provider === "anthropic" ? body.output_config?.format.schema
      : provider === "openai" ? body.text?.format.schema
      : body.generationConfig?.responseJsonSchema;
    assert.deepEqual(schema?.required, ["code"]);
    assert.equal(JSON.stringify(body).includes("tool_choice"), false);
  });

  test(`${provider}: truncated source is rejected instead of returned for execution, but its usage is still reported`, async () => {
    const { fetch } = transport(provider, [{ code: "console.log('partial');", truncated: true }]);
    const reports: GenerationReport[] = [];
    const generate = createGenerate(
      { provider, model: "test-model", apiKey: "test-key", maxOutputTokens: 10 }, fetch, (report) => reports.push(report),
    );
    await assert.rejects(generate({ instructions: "instructions", blocks: ["context"] }), /did not complete: length/);
    assert.equal(reports.length, 1);
    assert.equal(reports[0].finishReason, "length");
    assert.equal(reports[0].outputTokens, 5);
  });
}

test("Anthropic cache tracking stays inside the generator and handles long bursts and changed prefixes", async () => {
  const { requests, fetch } = transport("anthropic");
  const generate = createGenerate({ provider: "anthropic", model: "test-model", apiKey: "test-key", maxOutputTokens: 4096 }, fetch);
  const first = ["A", "B"];
  const extended = [...first, ...Array.from({ length: 40 }, (_, i) => `output ${i}`)];
  await generate({ instructions: "instructions", blocks: first });
  await generate({ instructions: "instructions", blocks: extended });
  await generate({ instructions: "changed instructions", blocks: extended });
  await generate({ instructions: "changed instructions", blocks: extended.slice(20) });
  const breakpoints = (index: number) => requests[index].body.messages![0].content
    .flatMap((part, i) => part.cache_control ? [i] : []);
  assert.deepEqual(breakpoints(0), [1]);
  assert.deepEqual(breakpoints(1), [1, 41]);
  assert.deepEqual(breakpoints(2), [41]);
  assert.deepEqual(breakpoints(3), [21]);
  assert.deepEqual(requests[0].body.system?.[0].cache_control, { type: "ephemeral" });
  assert.deepEqual(requests[1].body.messages![0].content.slice(0, 2), requests[0].body.messages![0].content);
});

test("a failed generation does not advance the cached prefix", async () => {
  const { requests, fetch } = transport("anthropic", [
    { code: "void 0" }, { code: "partial", truncated: true }, { code: "void 0" },
  ]);
  const generate = createGenerate({ provider: "anthropic", model: "test-model", apiKey: "test-key", maxOutputTokens: 4096 }, fetch);
  await generate({ instructions: "instructions", blocks: ["A"] });
  await assert.rejects(generate({ instructions: "instructions", blocks: ["A", "B"] }));
  await generate({ instructions: "instructions", blocks: ["A", "B", "C"] });
  assert.deepEqual(requests[2].body.messages![0].content.flatMap((part, i) => part.cache_control ? [i] : []), [0, 2]);
});

test("a complete reply that isn't the source object is rejected, and its usage still reported", async () => {
  const reports: GenerationReport[] = [];
  const fetch: typeof globalThis.fetch = async () => Response.json({
    ...response("anthropic", ""), content: [{ type: "text", text: '<invoke name="eval">' }],
  });
  const generate = createGenerate(
    { provider: "anthropic", model: "test-model", apiKey: "test-key", maxOutputTokens: 4096 }, fetch, (report) => reports.push(report),
  );
  await assert.rejects(generate({ instructions: "instructions", blocks: ["A"] }), /did not produce source/);
  assert.deepEqual(reports.map((report) => report.finishReason), ["stop"]);
});

test("Anthropic usage separates uncached, cached and output tokens", async () => {
  const { fetch } = transport("anthropic");
  const reports: GenerationReport[] = [];
  const generate = createGenerate(
    { provider: "anthropic", model: "test-model", apiKey: "test-key", maxOutputTokens: 4096 }, fetch, (report) => reports.push(report),
  );
  await generate({ instructions: "instructions", blocks: ["A"] });
  assert.deepEqual(
    [reports[0].finishReason, reports[0].uncachedInputTokens, reports[0].cacheReadTokens, reports[0].outputTokens],
    ["stop", 20, 10, 5],
  );
});

test("model configuration validates selection and only requires the selected provider's key", () => {
  const defaults = readModelConfig({ ANTHROPIC_API_KEY: "test-key" });
  assert.equal(defaults.model, "claude-sonnet-5-5");
  assert.deepEqual(defaults.pricing, { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2.5 });
  assert.equal(defaults.budgetUSD, 1);
  assert.equal(readModelConfig({ MODEL: "openai/custom/model", OPENAI_API_KEY: "test-key", MODEL_PRICING: "1,2" }).model, "custom/model");
  for (const MODEL of ["", "openai", "openai/", "unknown/model", "toString/model", "__proto__/model"]) {
    assert.throws(() => readModelConfig({ MODEL }), /MODEL must be/);
  }
  for (const provider of ["anthropic", "openai", "google"] as const) {
    assert.throws(() => readModelConfig({ MODEL: `${provider}/test-model` }), new RegExp(keys[provider]));
  }
  for (const value of ["0", "-1", "NaN", "1.5", "Infinity", ""]) {
    assert.throws(() => readModelConfig({ ANTHROPIC_API_KEY: "test-key", MODEL_MAX_OUTPUT_TOKENS: value }), /positive integer/);
  }
  for (const value of ["0", "-1", "NaN", "Infinity", "", "abc"]) {
    assert.throws(() => readModelConfig({ ANTHROPIC_API_KEY: "test-key", MODEL_BUDGET_USD: value }), /MODEL_BUDGET_USD/);
  }
  assert.equal(readModelConfig({ ANTHROPIC_API_KEY: "test-key", MODEL_BUDGET_USD: "0.25" }).budgetUSD, 0.25);
  // Spend can't be bounded for a model with unknown rates.
  assert.throws(() => readModelConfig({ MODEL: "openai/gpt-x", OPENAI_API_KEY: "test-key" }), /MODEL_PRICING/);
  assert.deepEqual(readModelConfig({ MODEL: "openai/gpt-x", OPENAI_API_KEY: "k", MODEL_PRICING: "1, 8, 0.1, 1" }).pricing,
    { input: 1, output: 8, cacheRead: 0.1, cacheWrite: 1 });
  assert.deepEqual(readModelConfig({ MODEL: "openai/gpt-x", OPENAI_API_KEY: "k", MODEL_PRICING: "1,8" }).pricing,
    { input: 1, output: 8, cacheRead: 1, cacheWrite: 1 });
  for (const value of ["", "1", "1,2,3", "1,-2", "a,b", "1,2,3,4,5"]) {
    assert.throws(() => readModelConfig({ MODEL: "openai/gpt-x", OPENAI_API_KEY: "k", MODEL_PRICING: value }), /MODEL_PRICING must/);
  }
});
