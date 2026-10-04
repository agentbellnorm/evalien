import type { DatabaseSync } from "node:sqlite";
import type { RecordStore, StoredRecord } from "../contracts.mts";

/** Append-only records in one SQLite table. Contents are opaque strings. */
export function createSqliteStore(db: DatabaseSync, table: string): RecordStore {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(table)) throw new TypeError(`Invalid table name: ${table}`);
  // Let observers read while the runtime appends.
  db.exec("PRAGMA busy_timeout = 1000; PRAGMA journal_mode = WAL;");
  db.exec(`CREATE TABLE IF NOT EXISTS ${table} (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    data TEXT NOT NULL
  )`);
  const insert = db.prepare(`INSERT INTO ${table} (timestamp, data) VALUES (?, ?)`);
  const latest = db.prepare(`SELECT COALESCE(MAX(seq), 0) AS seq FROM ${table}`);
  const one = db.prepare(`SELECT * FROM ${table} WHERE seq = ?`);
  const range = db.prepare(`SELECT * FROM ${table} WHERE seq > ? AND seq <= ? ORDER BY seq`);
  const row = (r: Record<string, unknown>): StoredRecord => ({
    seq: Number(r.seq), timestamp: String(r.timestamp), data: String(r.data),
  });

  return {
    append(data) {
      const timestamp = new Date().toISOString();
      const seq = Number(insert.run(timestamp, data).lastInsertRowid);
      return { seq, timestamp, data };
    },
    get(seq) {
      const found = one.get(seq);
      return found ? row(found) : undefined;
    },
    read: (after, through) => range.all(after, through).map(row),
    lastSeq: () => Number(latest.get()!.seq),
  };
}
