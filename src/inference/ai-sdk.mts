import { generateText, jsonSchema, NoObjectGeneratedError, Output, type LanguageModelUsage, type TextPart } from "ai";
import { createAnthropic } from "@ai-sdk/anthropic";
import { createOpenAI } from "@ai-sdk/openai";
import { createGoogle } from "@ai-sdk/google";
import { GenerationError, type Generate, type GenerateInput, type Generation } from "../contracts.mts";
import type { ModelConfig, Provider } from "./config.mts";
import { costOf } from "./pricing.mts";

const providers: Record<Provider, typeof createAnthropic | typeof createOpenAI | typeof createGoogle> = {
  anthropic: createAnthropic,
  openai: createOpenAI,
  google: createGoogle,
};

const sourceSchema = jsonSchema<{ code: string }>({
  type: "object",
  properties: { code: { type: "string", description: "JavaScript source for an async function body, or empty to do nothing until something happens" } },
  required: ["code"],
  additionalProperties: false,
});

/** SDK types, provider settings, and cache state stay behind Generate. */
export function createGenerate(
  config: ModelConfig,
  options: { fetch?: typeof globalThis.fetch; warn?: (message: string) => void } = {},
): Generate {
  const { fetch, warn } = options;
  const model = providers[config.provider]({
    apiKey: config.apiKey,
    baseURL: config.baseURL,
    fetch,
  }).languageModel(config.model);
  const anthropic = config.provider === "anthropic";
  const cacheOptions = { anthropic: { cacheControl: { type: "ephemeral" } } };
  let previous: GenerateInput | undefined;

  // SDK diagnostics are host output, not observations from evaluated code.
  globalThis.AI_SDK_LOG_WARNINGS = ({ warnings }) => warn?.(`model warnings: ${JSON.stringify(warnings)}`);

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
    const describe = (usage: LanguageModelUsage, finishReason: string): Generation => {
      const { inputTokens, inputTokenDetails, outputTokens, outputTokenDetails } = usage;
      const cacheReadTokens = inputTokenDetails.cacheReadTokens ?? 0;
      const cacheWriteTokens = inputTokenDetails.cacheWriteTokens ?? 0;
      const tokens = {
        uncachedInputTokens: inputTokenDetails.noCacheTokens ??
          Math.max(0, (inputTokens ?? 0) - cacheReadTokens - cacheWriteTokens),
        cacheReadTokens,
        cacheWriteTokens,
        outputTokens: outputTokens ?? 0,
        reasoningTokens: outputTokenDetails.reasoningTokens ?? 0,
      };
      return {
        model: `${config.provider}/${config.model}`,
        latencyMs: Date.now() - t0,
        finishReason,
        usage: { ...tokens, costUSD: costOf(tokens, config.pricing) },
      };
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
      // The SDK parses before returning, so incomplete or invalid replies land
      // here. They're still billed.
      if (!NoObjectGeneratedError.isInstance(err) || !err.usage) throw err;
      const generation = describe(err.usage, err.finishReason ?? "unknown");
      throw new GenerationError(generation.finishReason === "stop"
        ? `Generation did not produce source: ${err.message}`
        : `Generation did not complete: ${generation.finishReason}`, generation);
    }
    const generation = describe(result.usage, result.finishReason);
    if (generation.finishReason !== "stop") {
      throw new GenerationError(`Generation did not complete: ${generation.finishReason}`, generation);
    }
    previous = { instructions, blocks: snapshot };
    return { ...generation, code: result.output.code };
  };
}
