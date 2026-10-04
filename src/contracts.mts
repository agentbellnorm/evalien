// The shared vocabulary. Modules depend on these types, never on each other;
// the top-level entry points wire implementations together.

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
    }
  /** A model call. `through` is the last seq its prompt included. */
  | ({ type: "generation"; through: number } & Generation);

export type OutputEvent = Extract<TrajectoryEvent, { type: "stdout" | "stderr" }>;

/** Tokens by billing class, and their cost. */
export interface Usage {
  uncachedInputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  costUSD: number;
}

/** Why a model call ended. Only "stop" produced usable source. */
export const FINISH_REASONS = ["stop", "length", "content-filter", "tool-calls", "error", "other", "unknown"] as const;
export type FinishReason = (typeof FINISH_REASONS)[number];

/** What one model call cost and how it ended. */
export interface Generation {
  model: string;
  latencyMs: number;
  finishReason: FinishReason;
  usage: Usage;
}

export interface GenerateInput {
  instructions: string;
  blocks: readonly string[];
}

/** Completed JavaScript source, or empty to do nothing. */
export type Generate = (input: GenerateInput) => Promise<Generation & { code: string }>;

/** A billed call that produced no usable source. */
export class GenerationError extends Error {
  override name = "GenerationError";
  readonly generation: Generation;
  constructor(message: string, generation: Generation) {
    super(message);
    this.generation = generation;
  }
}

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

export interface Evaluation {
  outcome: "return" | "throw";
  /** The return value or error, captured as text. */
  text: string;
}

/** Runs model-generated source. Never rejects; failures are a "throw" outcome. */
export interface Evaluator {
  evaluate(code: string): Promise<Evaluation>;
}

/** Calls back for each line of human input. Returns a function that stops listening. */
export type InputSource = (onLine: (line: string) => void) => () => void;

/** Reports output written by running code. Returns a function that stops capturing. */
export type OutputSource = (emit: (event: OutputEvent) => void) => () => void;
