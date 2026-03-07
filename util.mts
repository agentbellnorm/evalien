import type { SQLInputValue } from "node:sqlite";
import { DatabaseSync } from "node:sqlite";

export function toError(value: unknown): Error {
  if (value instanceof Error) return value;
  return new Error(String(value));
}

export function dbGetNumber(db: DatabaseSync, sql: string, ...args: SQLInputValue[]): number {
  const row = db.prepare(sql).get(...args);
  const val = row ? Object.values(row)[0] : 0;
  return Number(val);
}

export function dbGetString(db: DatabaseSync, sql: string, ...args: SQLInputValue[]): string {
  const row = db.prepare(sql).get(...args);
  const val = row ? Object.values(row)[0] : "";
  return String(val);
}
