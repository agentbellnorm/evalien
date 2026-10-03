import { DatabaseSync } from "node:sqlite";
import { toError } from "./util.mts";

export interface EvalResult {
  result: unknown;
  error: string | null;
}

export function createContext(
  db: DatabaseSync,
): Record<string, unknown> {
  return {
    console,
    setTimeout,
    setInterval,
    clearTimeout,
    clearInterval,
    fetch,
    URL,
    Buffer,
    TextEncoder,
    TextDecoder,
    AbortController,
    btoa,
    atob,
    db,
  };
}

export async function evalCode(
  ctx: Record<string, unknown>,
  code: string,
): Promise<EvalResult> {
  try {
    const fn = new Function(
      "__ctx",
      `with(__ctx) { return (async () => { ${code} })() }`,
    );
    const result = await fn(ctx);
    return { result, error: null };
  } catch (err) {
    const e = toError(err);
    return { result: undefined, error: `${e.name}: ${e.message}` };
  }
}
