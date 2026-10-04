import type { Entry, EventLog, RecordStore, StoredRecord } from "../contracts.mts";
import { deserializeEvent, serializeEvent } from "./codec.mts";

function cursor(n: number): void {
  if (!Number.isSafeInteger(n) || n < 0) throw new TypeError("Trajectory cursors must be nonnegative integers");
}

const entry = ({ seq, timestamp, data }: StoredRecord): Entry => ({ seq, timestamp, event: deserializeEvent(data) });

/** The trajectory over any record store: validated on append, decoded on read. */
export function createTrajectory(store: RecordStore): EventLog {
  const listeners = new Set<(entry: Entry) => void>();
  let changed = Promise.withResolvers<void>();

  return {
    append(event) {
      const json = serializeEvent(event);
      const captured = deserializeEvent(json);
      if (captured.type === "result") {
        const evaluation = store.get(captured.evalId);
        if (!evaluation || deserializeEvent(evaluation.data).type !== "eval") {
          throw new TypeError(`Result references a missing evaluation: ${captured.evalId}`);
        }
      }
      const { seq, timestamp } = store.append(json);
      const appended = { seq, timestamp, event: captured };
      changed.resolve();
      changed = Promise.withResolvers<void>();
      for (const listener of listeners) listener(structuredClone(appended));
      return appended;
    },
    read(after = 0, through = store.lastSeq()) {
      cursor(after);
      cursor(through);
      return store.read(after, through).map(entry);
    },
    lastSeq: () => store.lastSeq(),
    async waitAfter(seq) {
      cursor(seq);
      let through: number;
      // No await between checking the log and subscribing. The log retains
      // arrivals while a consumer is busy; append() wakes consumers waiting here.
      while ((through = store.lastSeq()) <= seq) await changed.promise;
      return through;
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
  };
}
