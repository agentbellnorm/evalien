import { DatabaseSync } from "node:sqlite";
import { fmt, toError } from "./util.mts";

const EVAL_TIMEOUT = 30_000;

export interface EvalResult {
  logs: string[];
  result: unknown;
  error: string | null;
}

interface ConsoleProxy {
  log: (...args: unknown[]) => void;
  error: (...args: unknown[]) => void;
  warn: (...args: unknown[]) => void;
  info: (...args: unknown[]) => void;
}

// The shared execution context for all evals
export function createContext(db: DatabaseSync): Record<string, unknown> {
  return { console, setTimeout, setInterval, clearTimeout, clearInterval, fetch, URL, Buffer, TextEncoder, TextDecoder, AbortController, btoa, atob, db };
}

function captureConsole(): { proxy: ConsoleProxy; logs: string[] } {
  const logs: string[] = [];
  const make = (level: string) => (...args: unknown[]) => {
    const line = args.map(fmt).join(" ");
    logs.push(line);
    const color = level === "error" ? "31" : level === "warn" ? "33" : "0";
    process.stdout.write(`\x1b[${color}m  ${line}\x1b[0m\n`);
  };
  return {
    proxy: { log: make("log"), error: make("error"), warn: make("warn"), info: make("log") },
    logs,
  };
}

export async function evalCode(ctx: Record<string, unknown>, code: string): Promise<EvalResult> {
  const { proxy, logs } = captureConsole();
  const origConsole = ctx.console;
  ctx.console = proxy;

  try {
    const fn = new Function("__ctx", `with(__ctx) { return (async () => { ${code} })() }`);
    const result = await Promise.race([
      fn(ctx),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error("eval timed out after 30s")), EVAL_TIMEOUT)
      ),
    ]);
    ctx.console = origConsole;
    return { logs, result, error: null };
  } catch (err) {
    ctx.console = origConsole;
    const e = toError(err);
    return { logs, result: undefined, error: `${e.name}: ${e.message}` };
  }
}

export function formatEvalOutput({ logs, result, error }: EvalResult): string {
  const parts: string[] = [];
  if (logs.length > 0) parts.push(logs.join("\n"));
  if (result !== undefined) parts.push(`-> ${fmt(result)}`);
  if (error) parts.push(`x ${error}`);
  return parts.join("\n") || "(no output)";
}

export function parseAgentResponse(text: string): { eval: string } | null {
  const trimmed = text.trim();
  try {
    const parsed = JSON.parse(trimmed);
    if (parsed.eval !== undefined) return parsed;
  } catch {}

  const braceMatch = trimmed.match(/\{[\s\S]*"eval"[\s\S]*\}/);
  if (braceMatch) {
    try {
      const parsed = JSON.parse(braceMatch[0]);
      if (parsed.eval !== undefined) return parsed;
    } catch {}
  }

  return null;
}
