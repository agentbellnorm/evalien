import readline from "node:readline";
import type { DatabaseSync } from "node:sqlite";
import type { Readable } from "node:stream";
import { inspect } from "node:util";
import type { Generate } from "./generation.mts";
import { renderEntry } from "./trajectory.mts";
import { initTrajectory } from "./trajectory-store.mts";
import { createContext, evalCode } from "./eval.mts";
import { captureOutput, writeStdout, writeStderr } from "./output.mts";
import { debug, color, SYMBOL } from "./util.mts";

export interface RuntimeOptions {
  db: DatabaseSync;
  generate: Generate;
  instructions: string;
  input?: Readable;
}

/** One process owns one runtime. The caller owns the database connection. */
export async function runRuntime({ db, generate, instructions, input = process.stdin }: RuntimeOptions): Promise<void> {
  const trajectory = initTrajectory(db);
  const ctx = createContext(db);
  let cursor = trajectory.lastSeq();

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
      // Submitted source alone doesn't request inference. Every result does.
      if (observations.every(({ event }) => event.type === "eval")) continue;

      // Read a fixed prefix. Anything arriving during generation or evaluation
      // remains after the cursor for the next iteration.
      const after = Math.floor(Math.max(0, through - 500) / 200) * 200;
      const entries = trajectory.read(after, through);
      debug(`thinking... (through event ${through})`);
      const code = await generate({ instructions, blocks: entries.map(renderEntry) });

      const evaluation = trajectory.append({ type: "eval", code });
      writeStdout(color.dim(`${SYMBOL.bolt} ${code.replaceAll("\n", "\n  ")}`) + "\n");
      const result = await evalCode(ctx, code);
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
    }
  } finally {
    rl.close();
    restoreOutput();
  }
}
