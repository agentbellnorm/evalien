import { DatabaseSync } from "node:sqlite";
import { compile } from "./eval.mts";
import { deserializeEvent, renderEntry, type Entry } from "./trajectory.mts";

/** Quality bars for a run. A failing check exits nonzero. */
export const THRESHOLDS = {
  /** Share of evaluations whose source doesn't parse as JavaScript. */
  malformedRate: 0.05,
  /** Longest a human waits before a model call sees their input. */
  stdinWaitMs: 60_000,
  /** Share of input tokens served from cache, once a run has a few calls. */
  cacheReadShare: 0.5,
};

export interface Generation {
  id: number;
  timestamp: string;
  throughSeq: number;
  model: string;
  latencyMs: number;
  finishReason: string;
  uncachedInputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  costUSD: number;
}

export interface Check {
  name: string;
  pass: boolean;
  detail: string;
}

export interface Report {
  entries: Entry[];
  generations: Generation[];
  summary: Record<string, string | number>;
  checks: Check[];
}

export function readRun(db: DatabaseSync, lastRunOnly = false): { entries: Entry[]; generations: Generation[] } {
  let entries = db.prepare("SELECT * FROM trajectory ORDER BY seq").all().map((row): Entry => ({
    seq: Number(row.seq), timestamp: String(row.timestamp), event: deserializeEvent(String(row.event)),
  }));
  if (lastRunOnly) {
    const start = entries.findLastIndex(({ event }) => event.type === "start");
    entries = entries.slice(Math.max(0, start));
  }
  const hasGenerations = db.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'generations'").get();
  const since = entries[0]?.timestamp ?? "";
  const generations = !hasGenerations ? [] : db.prepare(
    "SELECT * FROM generations WHERE timestamp >= ? ORDER BY id",
  ).all(since).map((row): Generation => ({
    id: Number(row.id),
    timestamp: String(row.timestamp),
    throughSeq: Number(row.through_seq),
    model: String(row.model),
    latencyMs: Number(row.latency_ms),
    finishReason: String(row.finish_reason),
    uncachedInputTokens: Number(row.uncached_input_tokens),
    cacheReadTokens: Number(row.cache_read_tokens),
    cacheWriteTokens: Number(row.cache_write_tokens),
    outputTokens: Number(row.output_tokens),
    reasoningTokens: Number(row.reasoning_tokens),
    costUSD: Number(row.cost_usd),
  }));
  return { entries, generations };
}

function parses(code: string): boolean {
  try {
    compile(code);
    return true;
  } catch (err) {
    if (err instanceof SyntaxError) return false;
    throw err;
  }
}

/**
 * The eval each call produced, or null when it produced none: incomplete, or
 * an idle reply. A call's eval comes after its prompt and before the next
 * call's prompt ends, since the runtime reads past it before calling again.
 */
export function pairCalls(entries: Entry[], generations: Generation[]): Map<number, number | null> {
  return new Map(generations.map((g, i) => {
    const until = generations[i + 1]?.throughSeq ?? Infinity;
    const evaluation = g.finishReason === "stop"
      ? entries.find((e) => e.event.type === "eval" && e.seq > g.throughSeq && e.seq <= until)
      : undefined;
    return [g.id, evaluation?.seq ?? null];
  }));
}

const startedAt = (g: Generation) => new Date(Date.parse(g.timestamp) - g.latencyMs).toISOString();
const usd = (n: number) => `$${n.toFixed(4)}`;
const pct = (n: number) => `${(n * 100).toFixed(1)}%`;

export function analyze(entries: Entry[], generations: Generation[]): Report {
  const evals = entries.filter((entry) => entry.event.type === "eval");
  const malformed = evals.filter(({ event }) => event.type === "eval" && !parses(event.code));
  const thrown = entries.filter(({ event }) => event.type === "result" && event.outcome === "throw");
  const cost = generations.reduce((sum, g) => sum + g.costUSD, 0);
  const inputTokens = generations.reduce((sum, g) => sum + g.uncachedInputTokens + g.cacheReadTokens + g.cacheWriteTokens, 0);
  const cacheRead = generations.reduce((sum, g) => sum + g.cacheReadTokens, 0);
  const outputTokens = generations.reduce((sum, g) => sum + g.outputTokens, 0);
  const incomplete = generations.filter((g) => g.finishReason !== "stop");
  const pairs = pairCalls(entries, generations);
  const idle = generations.filter((g) => g.finishReason === "stop" && pairs.get(g.id) === null);

  // A model call saw input if its prompt ran through the input's seq. Runs
  // without logged generations fall back to the next evaluation's time.
  const waits = entries.filter((entry) => entry.event.type === "stdin").map((entry) => {
    const seen = generations.find((g) => g.throughSeq >= entry.seq);
    const next = generations.length > 0
      ? seen && startedAt(seen)
      : evals.find((e) => e.seq > entry.seq)?.timestamp;
    return { seq: entry.seq, waitMs: next ? Math.max(0, Date.parse(next) - Date.parse(entry.timestamp)) : Infinity };
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
  if (generations.length >= 3) {
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
  };
  if (generations.length === 0) {
    summary.generations = "not logged (run predates the generations table)";
  } else {
    Object.assign(summary, {
      generations: `${generations.length} (${idle.length} idle, ${incomplete.length} incomplete)`,
      models: [...new Set(generations.map((g) => g.model))].join(", "),
      cost: `${usd(cost)} (${usd(cost / generations.length)} per call)`,
      "input tokens": `${inputTokens} (${cacheRead} cached)`,
      "output tokens": outputTokens,
      "mean latency": `${Math.round(generations.reduce((sum, g) => sum + g.latencyMs, 0) / generations.length)}ms`,
    });
  }
  return { entries, generations, summary, checks };
}

/** Entries and model calls in the order they happened. */
export function renderTimeline({ entries, generations }: Pick<Report, "entries" | "generations">): string {
  const pairs = pairCalls(entries, generations);
  const render = (g: Generation) =>
    `[model ${g.timestamp} ${g.model} through=${g.throughSeq}] ${g.latencyMs}ms, ${g.finishReason}, ` +
    `in ${g.uncachedInputTokens} + ${g.cacheReadTokens} cached + ${g.cacheWriteTokens} written, ` +
    `out ${g.outputTokens} (${g.reasoningTokens} reasoning), ${usd(g.costUSD)}` +
    (g.finishReason === "stop" && pairs.get(g.id) === null ? ", idle" : "") + "\n\n";
  // A call precedes the eval it produced. Otherwise it follows what it saw.
  const before = new Map<number, string[]>();
  const after = new Map<number, string[]>();
  const trailing: string[] = [];
  const add = (map: Map<number, string[]>, seq: number, text: string) => map.set(seq, [...(map.get(seq) ?? []), text]);
  for (const g of generations) {
    const evaluation = pairs.get(g.id);
    if (evaluation != null) add(before, evaluation, render(g));
    else if (entries.some((e) => e.seq === g.throughSeq)) add(after, g.throughSeq, render(g));
    else trailing.push(render(g));
  }
  return entries.flatMap((entry) => [
    ...(before.get(entry.seq) ?? []), renderEntry(entry), ...(after.get(entry.seq) ?? []),
  ]).concat(trailing).join("");
}

export function renderReport(report: Report): string {
  const width = Math.max(...Object.keys(report.summary).map((key) => key.length));
  return [
    ...Object.entries(report.summary).map(([key, value]) => `${key.padEnd(width)}  ${value}`),
    "",
    ...report.checks.map((check) => `${check.pass ? "PASS" : "FAIL"}  ${check.name}: ${check.detail}`),
  ].join("\n") + "\n";
}

if (import.meta.main) {
  const args = process.argv.slice(2);
  const flags = new Set(args.filter((arg) => arg.startsWith("--")));
  const unknown = [...flags].filter((flag) => !["--timeline", "--last"].includes(flag));
  const paths = args.filter((arg) => !arg.startsWith("--"));
  if (unknown.length || paths.length > 1) {
    process.stderr.write("usage: report.mts [db] [--timeline] [--last]\n");
    process.exit(64);
  }
  const db = new DatabaseSync(paths[0] ?? process.env.AGENT_DB_PATH ?? "./agent.db", { readOnly: true });
  try {
    const { entries, generations } = readRun(db, flags.has("--last"));
    const report = analyze(entries, generations);
    if (flags.has("--timeline")) process.stdout.write(renderTimeline(report) + "\n");
    process.stdout.write(renderReport(report));
    process.exitCode = report.checks.every((check) => check.pass) ? 0 : 1;
  } finally {
    db.close();
  }
}
