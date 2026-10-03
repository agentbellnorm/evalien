import { generateText, type TextPart } from "ai";
import { createAnthropic } from "@ai-sdk/anthropic";
import { createOpenAI } from "@ai-sdk/openai";
import { createGoogle } from "@ai-sdk/google";
import type { Generate, GenerateInput } from "./generation.mts";
import { debug } from "./util.mts";

const providers = {
  anthropic: { create: createAnthropic, key: "ANTHROPIC_API_KEY" },
  openai: { create: createOpenAI, key: "OPENAI_API_KEY" },
  google: { create: createGoogle, key: "GOOGLE_GENERATIVE_AI_API_KEY" },
};

export interface ModelConfig {
  provider: keyof typeof providers;
  model: string;
  apiKey: string;
  baseURL?: string;
  maxOutputTokens: number;
}

/** Capture configuration once, before the runtime clears the environment. */
export function readModelConfig(env: Record<string, string | undefined>): ModelConfig {
  const selection = env.MODEL ?? "anthropic/claude-sonnet-4-6";
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
  return { provider: name, model, apiKey, baseURL: env.MODEL_BASE_URL, maxOutputTokens };
}

/** SDK types, provider settings, and cache state stay behind Generate. */
export function createGenerate(config: ModelConfig, fetch?: typeof globalThis.fetch): Generate {
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
    const result = await generateText({
      model,
      instructions: {
        role: "system", content: instructions,
        ...(anthropic ? { providerOptions: cacheOptions } : {}),
      },
      messages: [{ role: "user", content }],
      maxOutputTokens: config.maxOutputTokens,
    });
    const { inputTokens, inputTokenDetails } = result.usage;
    debug(`response in ${Date.now() - t0}ms | tokens: ${inputTokens ?? "?"} in, ${inputTokenDetails.cacheReadTokens ?? "?"} cached`);
    if (result.finishReason !== "stop") {
      throw new Error(`Generation did not complete: ${result.finishReason}`);
    }
    previous = { instructions, blocks: snapshot };
    return result.text;
  };
}
