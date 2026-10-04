import { inspect } from "node:util";
import type { Evaluator } from "../contracts.mts";

/** Globals every evaluation sees, before the ones the caller adds. */
const base = {
  console, setTimeout, setInterval, clearTimeout, clearInterval, fetch, URL,
  Buffer, TextEncoder, TextDecoder, AbortController, btoa, atob,
};

/** Parse without running. A SyntaxError here means the model's source was malformed. */
export function compile(code: string): (ctx: Record<string, unknown>) => Promise<unknown> {
  return new Function(
    "__ctx",
    `with(__ctx) { return (async () => { ${code} })() }`,
  ) as (ctx: Record<string, unknown>) => Promise<unknown>;
}

/** Whether source compiles. Exceptions it would throw at runtime don't count. */
export function parses(code: string): boolean {
  try {
    compile(code);
    return true;
  } catch (err) {
    if (err instanceof SyntaxError) return false;
    throw err;
  }
}

/**
 * Evaluates source as an async function body in this process. The context
 * object persists, so code can share state through it and globalThis.
 */
export function createEvaluator(globals: Record<string, unknown> = {}): Evaluator {
  const ctx: Record<string, unknown> = { ...base, ...globals };
  return {
    async evaluate(code) {
      try {
        const value = await compile(code)(ctx);
        return { outcome: "return", text: inspect(value, { depth: 4, colors: false, customInspect: false }) };
      } catch (err) {
        const e = err instanceof Error ? err : new Error(String(err));
        return { outcome: "throw", text: `${e.name}: ${e.message}` };
      }
    },
  };
}
