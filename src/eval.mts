import { DatabaseSync } from "node:sqlite";
import { fmt, toError, color } from "./util.mts";

const EVAL_TIMEOUT = 30_000;

export interface EvalResult {
  logs: string[];
  result: unknown;
  error: string | null;
}

export function createContext(
  db: DatabaseSync,
  say: (message: string) => void,
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
    say,
  };
}

function captureConsole(): {
  proxy: Record<string, (...args: unknown[]) => void>;
  logs: string[];
} {
  const logs: string[] = [];
  const make =
    (level: string) =>
    (...args: unknown[]) => {
      const line = args.map(fmt).join(" ");
      logs.push(line);
      const colorFn =
        level === "error"
          ? color.red
          : level === "warn"
            ? color.yellow
            : color.dim;
      process.stdout.write(colorFn(`  ${line}`) + "\n");
    };
  return {
    proxy: {
      log: make("log"),
      error: make("error"),
      warn: make("warn"),
      info: make("log"),
    },
    logs,
  };
}

export async function evalCode(
  ctx: Record<string, unknown>,
  code: string,
): Promise<EvalResult> {
  const { proxy, logs } = captureConsole();
  const origConsole = ctx.console;
  ctx.console = proxy;

  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const fn = new Function(
      "__ctx",
      `with(__ctx) { return (async () => { ${code} })() }`,
    );
    const result = await Promise.race([
      fn(ctx),
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("eval timed out after 30s")),
          EVAL_TIMEOUT,
        );
      }),
    ]);
    return { logs, result, error: null };
  } catch (err) {
    const e = toError(err);
    return { logs, result: undefined, error: `${e.name}: ${e.message}` };
  } finally {
    clearTimeout(timer);
    ctx.console = origConsole;
  }
}