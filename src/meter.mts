import type { DatabaseSync } from "node:sqlite";
import type { Generate } from "./generation.mts";

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

/** One completed model response, including responses rejected as incomplete. */
export interface GenerationReport {
  latencyMs: number;
  finishReason: string;
  uncachedInputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  reasoningTokens: number;
}

export function costOf(report: GenerationReport, pricing: Pricing): number {
  return (
    report.uncachedInputTokens * pricing.input +
    report.cacheReadTokens * pricing.cacheRead +
    report.cacheWriteTokens * pricing.cacheWrite +
    report.outputTokens * pricing.output
  ) / 1e6;
}

export class BudgetExceededError extends Error {
  override name = "BudgetExceededError";
}

export interface Meter {
  /** Record usage for every response the provider bills, before success is decided. */
  record(report: GenerationReport): void;
  /** Refuse to start another generation once this process has spent its budget. */
  guard(generate: Generate): Generate;
  spent(): number;
}

/**
 * Host bookkeeping. The budget is held in memory: evaluated code can edit the
 * generations table, but that doesn't reset the spend counted by this process.
 */
export function createMeter(db: DatabaseSync, options: {
  model: string;
  pricing: Pricing;
  budgetUSD: number;
  /** The latest trajectory seq, which is what a prompt built right now ends at. */
  position: () => number;
}): Meter {
  db.exec(`CREATE TABLE IF NOT EXISTS generations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    through_seq INTEGER NOT NULL,
    model TEXT NOT NULL,
    latency_ms INTEGER NOT NULL,
    finish_reason TEXT NOT NULL,
    uncached_input_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    reasoning_tokens INTEGER NOT NULL,
    cost_usd REAL NOT NULL
  )`);
  const insert = db.prepare(`INSERT INTO generations (
    timestamp, through_seq, model, latency_ms, finish_reason, uncached_input_tokens, cache_read_tokens,
    cache_write_tokens, output_tokens, reasoning_tokens, cost_usd
  ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  let spent = 0;
  // Captured when a generation starts; output arriving during it isn't in its prompt.
  let through: number | undefined;

  return {
    record(report) {
      const cost = costOf(report, options.pricing);
      spent += cost;
      insert.run(
        new Date().toISOString(), through ?? options.position(), options.model, report.latencyMs, report.finishReason,
        report.uncachedInputTokens, report.cacheReadTokens, report.cacheWriteTokens,
        report.outputTokens, report.reasoningTokens, cost,
      );
    },
    guard(generate) {
      return (input) => {
        if (spent >= options.budgetUSD) {
          return Promise.reject(new BudgetExceededError(
            `Spent $${spent.toFixed(4)} of the $${options.budgetUSD.toFixed(2)} MODEL_BUDGET_USD for this process`,
          ));
        }
        through = options.position();
        return generate(input).finally(() => { through = undefined; });
      };
    },
    spent: () => spent,
  };
}
