import { DatabaseSync } from "node:sqlite";

/** Open the database file. Readers and the appending runtime can work at once. */
export function openDatabase(path: string, { readOnly = false } = {}): DatabaseSync {
  const db = new DatabaseSync(path, { readOnly });
  db.exec("PRAGMA busy_timeout = 1000");
  if (!readOnly) db.exec("PRAGMA journal_mode = WAL");
  return db;
}
