import {
  GenerationError, type Entry, type Evaluator, type EventLog, type Generate, type InputSource, type OutputSource,
} from "../contracts.mts";
import { promptWindow } from "./context.mts";
import { fromHuman, IDLE_DELAYS, idleDelay, wakes } from "./policy.mts";

export interface RuntimeOptions {
  log: EventLog;
  generate: Generate;
  evaluator: Evaluator;
  input: InputSource;
  output: OutputSource;
  instructions: string;
  /** Minimum spacing between model calls with no human input since the last one. */
  idleDelays?: readonly number[];
  /** Host diagnostics; never part of the trajectory. */
  debug?: (message: string) => void;
}

/**
 * The harness: producers append observations, and one consumer decides when
 * to call the model and starts evaluating what it returns. Runs until
 * generation fails.
 */
export async function runRuntime({
  log, generate, evaluator, input, output, instructions, idleDelays = IDLE_DELAYS, debug = () => {},
}: RuntimeOptions): Promise<void> {
  let cursor = log.lastSeq();
  let pending = 0;
  let quiet = 0;
  let lastCall = 0;

  /** Hold a quiet call until its spacing has passed, or human input arrives. */
  async function pace(): Promise<void> {
    const deadline = lastCall + idleDelay(quiet, idleDelays);
    if (Date.now() >= deadline) return;
    debug(`pacing: quiet call ${quiet}, waiting ${Math.ceil((deadline - Date.now()) / 1000)}s unless input arrives`);
    for (;;) {
      let timer: NodeJS.Timeout | undefined;
      const elapsed = new Promise<null>((resolve) => { timer = setTimeout(resolve, deadline - Date.now(), null); });
      const next = await Promise.race([log.waitAfter(cursor), elapsed]);
      clearTimeout(timer);
      if (next === null) return;
      const arrived = log.read(cursor, next);
      cursor = next;
      if (arrived.some(({ event }) => fromHuman(event))) {
        quiet = 0;
        return;
      }
    }
  }

  /** Evaluation runs alongside later model calls; its result is an observation. */
  function evaluate(evaluation: Entry, code: string): void {
    pending++;
    void evaluator.evaluate(code).then(({ outcome, text }) => {
      pending--;
      log.append({ type: "result", evalId: evaluation.seq, outcome, text });
    }).catch((err) => debug(`result not recorded: ${String(err)}`));
  }

  const stopOutput = output((event) => { log.append(event); });
  const stopInput = input((text) => { log.append({ type: "stdin", text }); });
  try {
    log.append({ type: "start" });
    for (;;) {
      const latest = await log.waitAfter(cursor);
      const observations = log.read(cursor, latest);
      cursor = latest;
      if (!observations.some(({ event }) => wakes(event, pending))) continue;
      quiet = observations.some(({ event }) => fromHuman(event)) ? 0 : quiet + 1;
      if (quiet > 0) await pace();

      // Read a fixed prefix. Anything arriving during generation remains
      // after the cursor for the next iteration.
      const through = cursor;
      debug(`thinking... (through event ${through})`);
      lastCall = Date.now();
      let generated;
      try {
        generated = await generate({ instructions, blocks: promptWindow(log, through) });
      } catch (err) {
        if (err instanceof GenerationError) log.append({ type: "generation", through, ...err.generation });
        throw err;
      }
      const { code, ...generation } = generated;
      log.append({ type: "generation", through, ...generation });
      // No code ends the turn. The next waking observation starts another.
      if (!code.trim()) {
        debug("idle until the next event");
        continue;
      }
      evaluate(log.append({ type: "eval", code }), code);
      // Code that settles without I/O reports its result before the next decision.
      await new Promise((resolve) => setImmediate(resolve));
    }
  } finally {
    stopInput();
    stopOutput();
  }
}
