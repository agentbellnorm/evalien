import type { EventLog } from "../contracts.mts";
import { writeStderr, writeStdout } from "./capture.mts";
import { color, debug, SYMBOL } from "./format.mts";

type Write = (text: string) => unknown;

/**
 * Show the trajectory to the human as it grows. Output from running code
 * already reaches the terminal, so it isn't printed again.
 */
export function showTrajectory(log: EventLog, {
  out = writeStdout, err = writeStderr, note = debug,
}: { out?: Write; err?: Write; note?: Write } = {}): () => void {
  return log.subscribe(({ seq, event }) => {
    switch (event.type) {
      case "start":
        out(`evalien -- agent is waking up (${seq - 1} prior events)...\n\n`);
        break;
      case "stdin":
        out(color.green(`[you] ${event.text}`) + "\n");
        break;
      case "eval":
        out(color.dim(`${SYMBOL.bolt} ${event.code.replaceAll("\n", "\n  ")}`) + "\n");
        break;
      case "result":
        if (event.outcome === "throw") err(color.red(`${SYMBOL.cross} ${event.text}`) + "\n");
        else if (event.text !== "undefined") out(color.yellow(`${SYMBOL.arrow} ${event.text}`) + "\n");
        break;
      case "stdout":
      case "stderr":
        break;
      case "generation": {
        const { usage } = event;
        note(`${event.model} ${event.latencyMs}ms ${event.finishReason} | ${usage.uncachedInputTokens + usage.cacheReadTokens + usage.cacheWriteTokens} in, ` +
          `${usage.cacheReadTokens} cached, ${usage.outputTokens} out, $${usage.costUSD.toFixed(4)}`);
        break;
      }
      default: {
        const exhaustive: never = event;
        throw new TypeError(`Unknown trajectory event: ${String(exhaustive)}`);
      }
    }
  });
}
