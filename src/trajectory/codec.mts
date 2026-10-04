import { FINISH_REASONS, type FinishReason, type TrajectoryEvent, type Usage } from "../contracts.mts";

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

function count(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new TypeError("Expected a nonnegative integer");
  }
  return value;
}

function finishReason(value: unknown): FinishReason {
  const reason = FINISH_REASONS.find((known) => known === value);
  if (!reason) throw new TypeError(`Unknown finish reason: ${String(value)}`);
  return reason;
}

function usage(value: unknown): Usage {
  const u = record(value);
  fields(u, ["uncachedInputTokens", "cacheReadTokens", "cacheWriteTokens", "outputTokens", "reasoningTokens", "costUSD"]);
  if (typeof u.costUSD !== "number" || !Number.isFinite(u.costUSD) || u.costUSD < 0) {
    throw new TypeError("Expected a nonnegative cost");
  }
  return {
    uncachedInputTokens: count(u.uncachedInputTokens),
    cacheReadTokens: count(u.cacheReadTokens),
    cacheWriteTokens: count(u.cacheWriteTokens),
    outputTokens: count(u.outputTokens),
    reasoningTokens: count(u.reasoningTokens),
    costUSD: u.costUSD,
  };
}

function parseEvent(value: unknown): TrajectoryEvent {
  const event = record(value);
  // Typed so the switch is exhaustive; untrusted values still reach default.
  const type = event.type as TrajectoryEvent["type"];
  switch (type) {
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
      return { type, text: text(event.text) };
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
    case "generation":
      fields(event, ["type", "through", "model", "latencyMs", "finishReason", "usage"]);
      return {
        type: "generation",
        through: count(event.through),
        model: text(event.model),
        latencyMs: count(event.latencyMs),
        finishReason: finishReason(event.finishReason),
        usage: usage(event.usage),
      };
    default: {
      const unknown: never = type;
      throw new TypeError(`Unknown trajectory event: ${String(unknown)}`);
    }
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
