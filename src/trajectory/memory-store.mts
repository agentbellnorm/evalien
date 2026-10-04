import type { RecordStore, StoredRecord } from "../contracts.mts";

/** A RecordStore that lives and dies with the process. */
export function createMemoryStore(): RecordStore {
  const records: StoredRecord[] = [];
  return {
    append(data) {
      const record = { seq: records.length + 1, timestamp: new Date().toISOString(), data };
      records.push(record);
      return { ...record };
    },
    get: (seq) => (records[seq - 1] ? { ...records[seq - 1] } : undefined),
    read: (after, through) => records.slice(after, Math.max(after, through)).map((record) => ({ ...record })),
    lastSeq: () => records.length,
  };
}
