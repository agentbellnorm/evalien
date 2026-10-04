import type { Entry, TrajectoryEvent } from "../contracts.mts";

/** Quality bars for a run. A failing check exits nonzero. */
export const THRESHOLDS = {
  /** Share of evaluations whose source doesn't parse as JavaScript. */
  malformedRate: 0.05,
  /** Longest a human waits before a model call sees their input. */
  stdinWaitMs: 60_000,
  /** Share of input tokens served from cache, once a run has a few calls. */
  cacheReadShare: 0.5,
};

type Call = Entry & { event: Extract<TrajectoryEvent, { type: "generation" }> };

export interface Check {
  name: string;
  pass: boolean;
  detail: string;
}

export interface Report {
  entries: Entry[];
  summary: Record<string, string | number>;
  checks: Check[];
}

/** Entries from the latest start on. */
export function lastRun(entries: Entry[]): Entry[] {
  return entries.slice(Math.max(0, entries.findLastIndex(({ event }) => event.type === "start")));
}

const usd = (n: number) => `$${n.toFixed(4)}`;
const pct = (n: number) => `${(n * 100).toFixed(1)}%`;

/** Summarize a run and check it against THRESHOLDS. `parses` decides whether source is well-formed. */
export function analyze(entries: Entry[], { parses }: { parses: (code: string) => boolean }): Report {
  const calls = entries.filter((entry): entry is Call => entry.event.type === "generation");
  const evals = entries.filter((entry) => entry.event.type === "eval");
  const malformed = evals.filter(({ event }) => event.type === "eval" && !parses(event.code));
  const thrown = entries.filter(({ event }) => event.type === "result" && event.outcome === "throw");
  const sum = (pick: (call: Call["event"]) => number) => calls.reduce((total, { event }) => total + pick(event), 0);
  const cost = sum((g) => g.usage.costUSD);
  const cacheRead = sum((g) => g.usage.cacheReadTokens);
  const inputTokens = sum((g) => g.usage.uncachedInputTokens + g.usage.cacheReadTokens + g.usage.cacheWriteTokens);
  const incomplete = calls.filter(({ event }) => event.finishReason !== "stop");
  // The runtime appends a call's eval right after the call, so a completed
  // call without one was an idle reply.
  const idle = calls.filter((call) => call.event.finishReason === "stop" &&
    entries.find((e) => e.seq === call.seq + 1)?.event.type !== "eval");

  // A call saw input once its prompt ran through the input's seq.
  const startedAt = ({ timestamp, event }: Call) => Date.parse(timestamp) - event.latencyMs;
  const waits = entries.filter((entry) => entry.event.type === "stdin").map((entry) => {
    const seen = calls.find((call) => call.event.through >= entry.seq);
    return { seq: entry.seq, waitMs: seen ? Math.max(0, startedAt(seen) - Date.parse(entry.timestamp)) : Infinity };
  });
  const slow = waits.filter((w) => w.waitMs > THRESHOLDS.stdinWaitMs);

  const malformedRate = evals.length === 0 ? 0 : malformed.length / evals.length;
  const markup = malformed.filter(({ event }) => event.type === "eval" && /<invoke|```/.test(event.code));
  const checks: Check[] = [
    {
      name: "model output parses as JavaScript",
      pass: malformedRate <= THRESHOLDS.malformedRate,
      detail: `${malformed.length}/${evals.length} malformed (${pct(malformedRate)}, max ${pct(THRESHOLDS.malformedRate)})` +
        (malformed.length ? `; seq ${malformed.map((e) => e.seq).join(", ")}` : "") +
        (markup.length ? `; ${markup.length} contain tool-call markup or fences` : ""),
    },
    {
      name: "human input reaches the model promptly",
      pass: slow.length === 0,
      detail: waits.length === 0 ? "no input" : waits.map((w) =>
        `seq ${w.seq}: ${Number.isFinite(w.waitMs) ? `${(w.waitMs / 1000).toFixed(1)}s` : "never seen"}`).join(", ") +
        ` (max ${THRESHOLDS.stdinWaitMs / 1000}s)`,
    },
  ];
  if (calls.length >= 3) {
    const share = inputTokens === 0 ? 0 : cacheRead / inputTokens;
    checks.push({
      name: "prompt cache is reused",
      pass: share >= THRESHOLDS.cacheReadShare,
      detail: `${pct(share)} of input tokens read from cache (min ${pct(THRESHOLDS.cacheReadShare)})`,
    });
  }

  const summary: Record<string, string | number> = {
    events: entries.length,
    span: entries.length ? `${entries[0].timestamp} → ${entries.at(-1)!.timestamp}` : "empty",
    evaluations: evals.length,
    "thrown results": thrown.length,
    generations: `${calls.length} (${idle.length} idle, ${incomplete.length} incomplete)`,
  };
  if (calls.length > 0) {
    Object.assign(summary, {
      models: [...new Set(calls.map(({ event }) => event.model))].join(", "),
      cost: `${usd(cost)} (${usd(cost / calls.length)} per call)`,
      "input tokens": `${inputTokens} (${cacheRead} cached)`,
      "output tokens": sum((g) => g.usage.outputTokens),
      "mean latency": `${Math.round(sum((g) => g.latencyMs) / calls.length)}ms`,
    });
  }
  return { entries, summary, checks };
}

export function renderReport(report: Report): string {
  const width = Math.max(...Object.keys(report.summary).map((key) => key.length));
  return [
    ...Object.entries(report.summary).map(([key, value]) => `${key.padEnd(width)}  ${value}`),
    "",
    ...report.checks.map((check) => `${check.pass ? "PASS" : "FAIL"}  ${check.name}: ${check.detail}`),
  ].join("\n") + "\n";
}
