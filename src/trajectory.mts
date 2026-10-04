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

export interface Entry {
  seq: number;
  timestamp: string;
  event: TrajectoryEvent;
}

export type OutputEvent = Extract<TrajectoryEvent, { type: "stdout" | "stderr" }>;

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError("Expected a trajectory record");
  }
  return value as Record<string, unknown>;
}

function fields(value: Record<string, unknown>, names: string[]): void {
  if (
    Object.keys(value).length !== names.length ||
    names.some((name) => !Object.hasOwn(value, name))
  ) {
    throw new TypeError(`Expected trajectory fields: ${names.join(", ")}`);
  }
}

function text(value: unknown): string {
  if (typeof value !== "string") throw new TypeError("Expected trajectory text");
  return value;
}

function parseEvent(value: unknown): TrajectoryEvent {
  const event = record(value);
  switch (event.type) {
    case "start":
      fields(event, ["type"]);
      return { type: "start" };
    case "eval":
      fields(event, ["type", "code"]);
      return { type: "eval", code: text(event.code) };
    case "stdin":
    case "stdout":
    case "stderr":
      fields(event, ["type", "text"]);
      return { type: event.type, text: text(event.text) };
    case "result":
      fields(event, ["type", "evalId", "outcome", "text"]);
      if (
        typeof event.evalId !== "number" ||
        !Number.isSafeInteger(event.evalId) ||
        event.evalId <= 0
      ) {
        throw new TypeError("Expected a positive evaluation ID");
      }
      if (event.outcome !== "return" && event.outcome !== "throw") {
        throw new TypeError("Expected a return or throw outcome");
      }
      return {
        type: "result",
        evalId: event.evalId,
        outcome: event.outcome,
        text: text(event.text),
      };
    default:
      throw new TypeError(`Unknown trajectory event: ${String(event.type)}`);
  }
}

/** Canonical, versioned JSON; never serializes a live runtime object. */
export function serializeEvent(event: TrajectoryEvent): string {
  return JSON.stringify({ version: 1, event: parseEvent(event) });
}

export function deserializeEvent(json: string): TrajectoryEvent {
  const envelope = record(JSON.parse(json));
  fields(envelope, ["version", "event"]);
  if (envelope.version !== 1) {
    throw new TypeError(`Unsupported trajectory version: ${String(envelope.version)}`);
  }
  return parseEvent(envelope.event);
}

/** Larger bodies keep their head and tail; the full text stays in the database. */
export const MAX_ENTRY_CHARS = 8_000;

function clip(seq: number, body: string): string {
  if (body.length <= MAX_ENTRY_CHARS) return body;
  const half = MAX_ENTRY_CHARS / 2;
  return `${body.slice(0, half)}\n… ${body.length - MAX_ENTRY_CHARS} characters omitted; full event: SELECT event FROM trajectory WHERE seq = ${seq} …\n${body.slice(-half)}`;
}

/** Each entry renders independently, so appending preserves the existing text. */
export function renderEntry({ seq, timestamp, event }: Entry): string {
  let label: string = event.type;
  let body: string;
  switch (event.type) {
    case "start":
      body = "Fresh runtime. Previous live bindings, callbacks, and connections are gone.";
      break;
    case "eval":
      body = event.code;
      break;
    case "stdin":
    case "stdout":
    case "stderr":
      body = event.text;
      break;
    case "result":
      label = `result eval=${event.evalId} ${event.outcome}`;
      body = event.text;
      break;
    default: {
      const exhaustive: never = event;
      throw new TypeError(`Unknown trajectory event: ${exhaustive}`);
    }
  }
  return `[${seq} ${timestamp} ${label}]\n${clip(seq, body).split("\n").map((line) => `| ${line}`).join("\n")}\n\n`;
}

export function renderTrajectory(entries: readonly Entry[]): string {
  return entries.map(renderEntry).join("");
}
