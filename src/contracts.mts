// The shared vocabulary. Modules depend on these types, never on each other;
// main.mts wires implementations together.

/** Observable events at the runtime boundary. Values are captured as text. */
export type TrajectoryEvent =
  | { type: "start" }
  | { type: "eval"; code: string }
  | { type: "stdin"; text: string }
  | { type: "stdout"; text: string }
  | { type: "stderr"; text: string }
  | {
      type: "result";
      evalId: number;
      outcome: "return" | "throw";
      text: string;
    };

export type OutputEvent = Extract<TrajectoryEvent, { type: "stdout" | "stderr" }>;

export interface Entry {
  seq: number;
  timestamp: string;
  event: TrajectoryEvent;
}

/** A stored record. The store assigns sequence and time; contents are opaque. */
export interface StoredRecord {
  seq: number;
  timestamp: string;
  data: string;
}

/** Durable, append-only records in sequence order. */
export interface RecordStore {
  append(data: string): StoredRecord;
  get(seq: number): StoredRecord | undefined;
  /** Records in (after, through], in sequence order. */
  read(after: number, through: number): StoredRecord[];
  lastSeq(): number;
}

/** The trajectory: validated events over a record store. */
export interface EventLog {
  append(event: TrajectoryEvent): Entry;
  /** Read (after, through], in observation order. */
  read(after?: number, through?: number): Entry[];
  lastSeq(): number;
  /** Return the latest sequence after this cursor, waiting for append() if needed. */
  waitAfter(seq: number): Promise<number>;
  /** Called synchronously for each appended entry, in order. */
  subscribe(listener: (entry: Entry) => void): () => void;
}
