import { generateText, jsonSchema, NoObjectGeneratedError, Output, type LanguageModelUsage, type TextPart } from "ai";
import { createAnthropic } from "@ai-sdk/anthropic";
import { createOpenAI } from "@ai-sdk/openai";
import { createGoogle } from "@ai-sdk/google";
import type { Generate, GenerateInput } from "./generation.mts";
import { readPricing, type GenerationReport, type Pricing } from "./meter.mts";
import { debug } from "./util.mts";

const providers = {
  anthropic: { create: createAnthropic, key: "ANTHROPIC_API_KEY" },
  openai: { create: createOpenAI, key: "OPENAI_API_KEY" },
  google: { create: createGoogle, key: "GOOGLE_GENERATIVE_AI_API_KEY" },
};

const sourceSchema = jsonSchema<{ code: string }>({
  type: "object",
  properties: { code: { type: "string", description: "JavaScript source for an async function body, or empty to do nothing until something happens" } },
  required: ["code"],
  additionalProperties: false,
});

export interface ModelConfig {
  provider: keyof typeof providers;
  model: string;
  apiKey: string;
  baseURL?: string;
  maxOutputTokens: number;
}

export interface RunConfig extends ModelConfig {
  pricing: Pricing;
  budgetUSD: number;
}

/** Capture configuration once, before the runtime clears the environment. */
export function readModelConfig(env: Record<string, string | undefined>): RunConfig {
  const selection = env.MODEL ?? "anthropic/claude-sonnet-5-5";
  const slash = selection.indexOf("/");
  const provider = selection.slice(0, slash);
  const model = selection.slice(slash + 1).trim();
  if (slash < 1 || !Object.hasOwn(providers, provider) || !model) {
    throw new Error("MODEL must be anthropic/<model>, openai/<model>, or google/<model>");
  }
  const name = provider as keyof typeof providers;
  const key = providers[name].key;
  const apiKey = env[key]?.trim();
  if (!apiKey) throw new Error(`Set ${key} for MODEL=${selection}`);
  const maxOutputTokens = Number(env.MODEL_MAX_OUTPUT_TOKENS ?? 4096);
  if (!Number.isSafeInteger(maxOutputTokens) || maxOutputTokens <= 0) {
    throw new Error("MODEL_MAX_OUTPUT_TOKENS must be a positive integer");
  }
  const budgetUSD = Number(env.MODEL_BUDGET_USD ?? 1);
  if (!Number.isFinite(budgetUSD) || budgetUSD <= 0) {
    throw new Error("MODEL_BUDGET_USD must be a positive number");
  }
  return {
    provider: name, model, apiKey, baseURL: env.MODEL_BASE_URL, maxOutputTokens,
    pricing: readPricing(`${name}/${model}`, env.MODEL_PRICING), budgetUSD,
  };
}

/** SDK types, provider settings, and cache state stay behind Generate. */
export function createGenerate(
  config: ModelConfig,
  fetch?: typeof globalThis.fetch,
  onReport?: (report: GenerationReport) => void,
): Generate {
  const model = providers[config.provider].create({
    apiKey: config.apiKey,
    baseURL: config.baseURL,
    fetch,
  }).languageModel(config.model);
  const anthropic = config.provider === "anthropic";
  const cacheOptions = { anthropic: { cacheControl: { type: "ephemeral" } } };
  let previous: GenerateInput | undefined;

  // SDK diagnostics are host output, not observations from evaluated code.
  globalThis.AI_SDK_LOG_WARNINGS = ({ warnings }) => debug(`model warnings: ${JSON.stringify(warnings)}`);

  return async ({ instructions, blocks }) => {
    const snapshot = [...blocks];
    if (snapshot.length === 0) throw new Error("Generation requires at least one prompt block");
    const extendsPrevious = previous?.instructions === instructions &&
      previous.blocks.length <= snapshot.length &&
      previous.blocks.every((block, i) => block === snapshot[i]);
    const previousEnd = extendsPrevious ? previous!.blocks.length - 1 : -1;
    const content: TextPart[] = snapshot.map((text, i) => ({
      type: "text", text,
      ...(anthropic && (i === previousEnd || i === snapshot.length - 1)
        ? { providerOptions: cacheOptions } : {}),
    }));
    const t0 = Date.now();
    const report = (usage: LanguageModelUsage, finishReason: string) => {
      const latencyMs = Date.now() - t0;
      const { inputTokens, inputTokenDetails, outputTokens, outputTokenDetails } = usage;
      const cacheReadTokens = inputTokenDetails.cacheReadTokens ?? 0;
      const cacheWriteTokens = inputTokenDetails.cacheWriteTokens ?? 0;
      // Billed even when the response is rejected below.
      onReport?.({
        latencyMs,
        finishReason,
        uncachedInputTokens: inputTokenDetails.noCacheTokens ??
          Math.max(0, (inputTokens ?? 0) - cacheReadTokens - cacheWriteTokens),
        cacheReadTokens,
        cacheWriteTokens,
        outputTokens: outputTokens ?? 0,
        reasoningTokens: outputTokenDetails.reasoningTokens ?? 0,
      });
      debug(`response in ${latencyMs}ms | tokens: ${inputTokens ?? "?"} in, ${cacheReadTokens} cached, ${outputTokens ?? "?"} out`);
      if (finishReason !== "stop") throw new Error(`Generation did not complete: ${finishReason}`);
    };
    let result;
    try {
      result = await generateText({
        model,
        instructions: {
          role: "system", content: instructions,
          ...(anthropic ? { providerOptions: cacheOptions } : {}),
        },
        messages: [{ role: "user", content }],
        maxOutputTokens: config.maxOutputTokens,
        // Constrained decoding where the provider supports it: the reply is
        // this object, never prose, markup, or fenced source.
        output: Output.object({ schema: sourceSchema, name: "evaluation" }),
        // Never the forced JSON-tool fallback, which current Claude models reject.
        ...(anthropic ? { providerOptions: { anthropic: { structuredOutputMode: "outputFormat" } } } : {}),
      });
    } catch (err) {
      // The SDK parses before returning, so incomplete or invalid replies land here.
      if (!NoObjectGeneratedError.isInstance(err) || !err.usage) throw err;
      report(err.usage, err.finishReason ?? "unknown");
      throw new Error(`Generation did not produce source: ${err.message}`);
    }
    report(result.usage, result.finishReason);
    previous = { instructions, blocks: snapshot };
    return result.output.code;
  };
}
