import type { Usage } from "../contracts.mts";

/** USD per million tokens. */
export interface Pricing {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

// Only rates confirmed for the model ID. Other models need MODEL_PRICING.
const knownPricing: Record<string, Pricing> = {
  "anthropic/claude-sonnet-5-5": { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2.5 },
  "anthropic/claude-opus-5-5": { input: 4, output: 20, cacheRead: 0.2, cacheWrite: 5 },
};

/** Resolve pricing from MODEL_PRICING ("input,output[,cacheRead,cacheWrite]") or the known table. */
export function readPricing(selection: string, override: string | undefined): Pricing {
  if (override === undefined) {
    const known = knownPricing[selection];
    if (!known) {
      throw new Error(`Set MODEL_PRICING="input,output,cacheRead,cacheWrite" (USD per million tokens) for MODEL=${selection}`);
    }
    return known;
  }
  const rates = override.split(",").map((part) => Number(part.trim()));
  if (![2, 4].includes(rates.length) || rates.some((rate) => !Number.isFinite(rate) || rate < 0)) {
    throw new Error("MODEL_PRICING must be 2 or 4 nonnegative numbers: input,output[,cacheRead,cacheWrite]");
  }
  const [input, output, cacheRead = input, cacheWrite = input] = rates;
  return { input, output, cacheRead, cacheWrite };
}

export function costOf(tokens: Omit<Usage, "costUSD">, pricing: Pricing): number {
  return (
    tokens.uncachedInputTokens * pricing.input +
    tokens.cacheReadTokens * pricing.cacheRead +
    tokens.cacheWriteTokens * pricing.cacheWrite +
    tokens.outputTokens * pricing.output
  ) / 1e6;
}
