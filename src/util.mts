import { inspect } from "node:util";

export function toError(value: unknown): Error {
  if (value instanceof Error) return value;
  return new Error(String(value));
}

export function fmt(val: unknown): string {
  return typeof val === "string"
    ? val
    : inspect(val, { depth: 4, colors: false });
}

// ANSI color helpers
const RESET = "\x1b[0m";
const DIM = "\x1b[90m";
const RED = "\x1b[31m";
const GREEN = "\x1b[32m";
const YELLOW = "\x1b[33m";
const CYAN = "\x1b[36m";

export function color(code: string, text: string): string {
  return `${code}${text}${RESET}`;
}

color.dim = (text: string) => color(DIM, text);
color.red = (text: string) => color(RED, text);
color.green = (text: string) => color(GREEN, text);
color.yellow = (text: string) => color(YELLOW, text);
color.cyan = (text: string) => color(CYAN, text);

// Unicode symbols
export const SYMBOL = {
  bolt: "\u26a1",
  cross: "\u2718",
  arrow: "\u2192",
} as const;

export function debug(msg: string): void {
  process.stderr.write(
    color.dim(`[${new Date().toISOString().slice(11, 19)}] ${msg}`) + "\n",
  );
}
