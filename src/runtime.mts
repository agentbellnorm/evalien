import readline from "node:readline";
import type { DatabaseSync } from "node:sqlite";
import type { Readable } from "node:stream";
import { inspect } from "node:util";
import type { Generate } from "./generation.mts";
import { renderEntry, type Entry } from "./trajectory.mts";
import { initTrajectory, type Trajectory } from "./trajectory-store.mts";
import { createContext, evalCode } from "./eval.mts";
import { captureOutput, writeStdout, writeStderr } from "./output.mts";
import { debug, color, SYMBOL } from "./util.mts";

export interface RuntimeOptions {
  db: DatabaseSync;
  generate: Generate;
  instructions: string;
  input?: Readable;
  /** Minimum spacing between model calls with no human input since the last one. */
  idleDelays?: readonly number[];
}

/**
 * The first few model calls without human input run back to back, so the
 * agent can work. Further calls get further apart until input arrives.
 */
export const IDLE_DELAYS: readonly number[] = [0, 0, 0, 5_000, 30_000, 120_000, 600_000];

/** Bounds input cost per call, even when evaluated code floods output. */
export const MAX_PROMPT_CHARS = 400_000;

/**
 * A recent window whose start moves in fixed steps, so consecutive prompts
 * share a prefix. Only an oversized window drops older entries beyond that.
 */
export function promptWindow(trajectory: Trajectory, through: number): string[] {
  let after = Math.floor(Math.max(0, through - 500) / 200) * 200;
  let blocks = trajectory.read(after, through).map(renderEntry);
  const size = () => blocks.reduce((total, block) => total + block.length, 0);
  while (after + 200 < through && size() > MAX_PROMPT_CHARS) {
    after += 200;
    blocks = trajectory.read(after, through).map(renderEntry);
  }
  while (blocks.length > 1 && size() > MAX_PROMPT_CHARS) blocks.shift();
  return blocks;
}

/** One process owns one runtime. The caller owns the database connection. */
export async function runRuntime({
  db, generate, instructions, input = process.stdin, idleDelays = IDLE_DELAYS,
}: RuntimeOptions): Promise<void> {
  const trajectory = initTrajectory(db);
  const ctx = createContext(db);
  let cursor = trajectory.lastSeq();
  let pending = 0;
  let quiet = 0;
  let lastCall = 0;
  const human = ({ event }: Entry) => event.type === "stdin" || event.type === "start";
  // Results and input always request inference. Output does when nothing is
  // pending; a pending evaluation's output is seen with its result.
  const wakes = ({ event }: Entry) => event.type === "result" || event.type === "stdin" ||
    event.type === "start" || (pending === 0 && (event.type === "stdout" || event.type === "stderr"));

  /** Hold a quiet call until its spacing has passed, or human input arrives. */
  async function pace(): Promise<void> {
    const delay = idleDelays[Math.min(quiet, idleDelays.length) - 1] ?? 0;
    const deadline = lastCall + delay;
    if (Date.now() >= deadline) return;
    debug(`pacing: quiet call ${quiet}, waiting ${Math.ceil((deadline - Date.now()) / 1000)}s unless input arrives`);
    for (;;) {
      let timer: NodeJS.Timeout | undefined;
      const elapsed = new Promise<null>((resolve) => { timer = setTimeout(resolve, deadline - Date.now(), null); });
      const next = await Promise.race([trajectory.waitAfter(cursor), elapsed]);
      clearTimeout(timer);
      if (next === null) return;
      const arrived = trajectory.read(cursor, next);
      cursor = next;
      if (arrived.some(human)) {
        quiet = 0;
        return;
      }
    }
  }

  function evaluate(evaluation: Entry, code: string): void {
    pending++;
    void evalCode(ctx, code).then((result) => {
      pending--;
      const text = result.error ?? inspect(result.result, {
        depth: 4, colors: false, customInspect: false,
      });
      trajectory.append({
        type: "result",
        evalId: evaluation.seq,
        outcome: result.error === null ? "return" : "throw",
        text,
      });
      if (result.error !== null) writeStderr(color.red(`${SYMBOL.cross} ${text}`) + "\n");
      else if (result.result !== undefined) writeStdout(color.yellow(`${SYMBOL.arrow} ${text}`) + "\n");
    }).catch((err) => debug(`result not recorded: ${String(err)}`));
  }

  // Producers only append observations. The consumer below decides when to infer.
  const restoreOutput = captureOutput((event) => { trajectory.append(event); });
  const rl = readline.createInterface({ input, prompt: "" });
  rl.on("line", (line: string) => {
    if (!line.trim()) return;
    writeStdout(color.green(`[you] ${line}`) + "\n");
    trajectory.append({ type: "stdin", text: line });
  });

  try {
    trajectory.append({ type: "start" });
    writeStdout(`evalien -- agent is waking up (${cursor} prior events)...\n\n`);

    for (;;) {
      const through = await trajectory.waitAfter(cursor);
      const observations = trajectory.read(cursor, through);
      cursor = through;
      if (!observations.some(wakes)) continue;
      quiet = observations.some(human) ? 0 : quiet + 1;
      if (quiet > 0) await pace();

      // Read a fixed prefix. Anything arriving during generation remains
      // after the cursor for the next iteration.
      const blocks = promptWindow(trajectory, cursor);
      debug(`thinking... (through event ${cursor})`);
      lastCall = Date.now();
      const code = await generate({ instructions, blocks });
      // No code ends the turn. The next waking observation starts another.
      if (!code.trim()) {
        debug("idle until the next event");
        continue;
      }

      // Evaluation runs alongside later generations; its result is an observation.
      const evaluation = trajectory.append({ type: "eval", code });
      writeStdout(color.dim(`${SYMBOL.bolt} ${code.replaceAll("\n", "\n  ")}`) + "\n");
      evaluate(evaluation, code);
      // Code that settles without I/O reports its result before the next decision.
      await new Promise((resolve) => setImmediate(resolve));
    }
  } finally {
    rl.close();
    restoreOutput();
  }
}
