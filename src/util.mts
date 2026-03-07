import { inspect } from "node:util";

export function toError(value: unknown): Error {
  if (value instanceof Error) return value;
  return new Error(String(value));
}

export function fmt(val: unknown): string {
  return typeof val === "string" ? val : inspect(val, { depth: 4, colors: false });
}

export function debug(msg: string): void {
  process.stderr.write(`\x1b[90m[${new Date().toISOString().slice(11, 19)}] ${msg}\x1b[0m\n`);
}

export function printLines(lines: string[], color = "36"): void {
  for (const line of lines) {
    process.stdout.write(`\x1b[${color}m${line}\x1b[0m\n`);
  }
}
