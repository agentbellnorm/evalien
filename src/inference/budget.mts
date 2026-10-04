import { GenerationError, type Generate } from "../contracts.mts";

export class BudgetExceededError extends Error {
  override name = "BudgetExceededError";
}

/**
 * Refuse to start a call once this process has spent its budget. The call
 * that crosses the limit completes. Spend lives in memory, out of reach of
 * evaluated code that edits stored history.
 */
export function withBudget(generate: Generate, budgetUSD: number): { generate: Generate; spent(): number } {
  let spent = 0;
  return {
    async generate(input) {
      if (spent >= budgetUSD) {
        throw new BudgetExceededError(`Spent $${spent.toFixed(4)} of this process's $${budgetUSD.toFixed(2)} budget`);
      }
      try {
        const result = await generate(input);
        spent += result.usage.costUSD;
        return result;
      } catch (err) {
        if (err instanceof GenerationError) spent += err.generation.usage.costUSD;
        throw err;
      }
    },
    spent: () => spent,
  };
}
