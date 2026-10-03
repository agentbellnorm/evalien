import type { DatabaseSync } from "node:sqlite";
import { deserializeEvent, serializeEvent, type Entry, type TrajectoryEvent } from "./trajectory.mts";

export interface Trajectory {
  append(event: TrajectoryEvent): Entry;
  lastSeq(): number;
  /** Read (after, through], in observation order. */
  read(after?: number, through?: number): Entry[];
  /** Return the latest sequence after this cursor, waiting for append() on this instance if needed. */
  waitAfter(seq: number): Promise<number>;
}

export function initTrajectory(db: DatabaseSync): Trajectory {
  // Let observers read while the runtime appends observations.
  db.exec("PRAGMA busy_timeout = 1000; PRAGMA journal_mode = WAL;");
  db.exec(`CREATE TABLE IF NOT EXISTS trajectory (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    event TEXT NOT NULL
  )`);

  const insert = db.prepare("INSERT INTO trajectory (timestamp, event) VALUES (?, ?)");
  const latest = db.prepare("SELECT COALESCE(MAX(seq), 0) AS seq FROM trajectory");
  const select = db.prepare("SELECT * FROM trajectory WHERE seq > ? AND seq <= ? ORDER BY seq");
  const find = db.prepare("SELECT event FROM trajectory WHERE seq = ?");
  const lastSeq = (): number => Number(latest.get()!.seq);
  let changed = Promise.withResolvers<void>();

  return {
    append(event): Entry {
      const json = serializeEvent(event);
      const captured = deserializeEvent(json);
      if (captured.type === "result") {
        const evaluation = find.get(captured.evalId);
        if (!evaluation || deserializeEvent(String(evaluation.event)).type !== "eval") {
          throw new TypeError(`Result references a missing evaluation: ${captured.evalId}`);
        }
      }
      const timestamp = new Date().toISOString();
      const seq = Number(insert.run(timestamp, json).lastInsertRowid);
      changed.resolve();
      changed = Promise.withResolvers<void>();
      return { seq, timestamp, event: captured };
    },
    lastSeq,
    read(after = 0, through = lastSeq()): Entry[] {
      if (![after, through].every((n) => Number.isSafeInteger(n) && n >= 0)) {
        throw new TypeError("Trajectory cursors must be nonnegative integers");
      }
      return select.all(after, through).map((row) => ({
        seq: Number(row.seq),
        timestamp: String(row.timestamp),
        event: deserializeEvent(String(row.event)),
      }));
    },
    async waitAfter(seq): Promise<number> {
      if (!Number.isSafeInteger(seq) || seq < 0) {
        throw new TypeError("Trajectory cursors must be nonnegative integers");
      }
      let through: number;
      // No await between checking the log and subscribing. The log retains
      // arrivals while a consumer is busy; append() wakes consumers waiting here.
      while ((through = lastSeq()) <= seq) await changed.promise;
      return through;
    },
  };
}
