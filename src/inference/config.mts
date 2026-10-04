import { readPricing, type Pricing } from "./pricing.mts";

export const providerKeys = {
  anthropic: "ANTHROPIC_API_KEY",
  openai: "OPENAI_API_KEY",
  google: "GOOGLE_GENERATIVE_AI_API_KEY",
};
export type Provider = keyof typeof providerKeys;

export interface ModelConfig {
  provider: Provider;
  model: string;
  apiKey: string;
  baseURL?: string;
  maxOutputTokens: number;
  pricing: Pricing;
}

export interface InferenceConfig extends ModelConfig {
  budgetUSD: number;
}

/** Parse inference settings from environment-style variables. */
export function readInferenceConfig(env: Record<string, string | undefined>): InferenceConfig {
  const selection = env.MODEL ?? "anthropic/claude-sonnet-5-5";
  const slash = selection.indexOf("/");
  const provider = selection.slice(0, slash);
  const model = selection.slice(slash + 1).trim();
  if (slash < 1 || !Object.hasOwn(providerKeys, provider) || !model) {
    throw new Error("MODEL must be anthropic/<model>, openai/<model>, or google/<model>");
  }
  const name = provider as Provider;
  const key = providerKeys[name];
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
