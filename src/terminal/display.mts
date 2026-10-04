import type { EventLog } from "../contracts.mts";
import { writeStderr, writeStdout } from "./capture.mts";
import { color, debug, SYMBOL } from "./format.mts";

/**
 * Show the trajectory to the human as it grows. Output from running code
 * already reaches the terminal, so only the rest is printed.
 */
export function showTrajectory(log: EventLog, {
  out = writeStdout, err = writeStderr, note = debug,
}: { out?: (text: string) => unknown; err?: (text: string) => unknown; note?: (text: string) => unknown } = {}): () => void {
  return log.subscribe(({ event }) => {
    switch (event.type) {
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
      case "generation": {
        const { usage } = event;
        note(`${event.model} ${event.latencyMs}ms ${event.finishReason} | ${usage.uncachedInputTokens + usage.cacheReadTokens + usage.cacheWriteTokens} in, ` +
          `${usage.cacheReadTokens} cached, ${usage.outputTokens} out, $${usage.costUSD.toFixed(4)}`);
        break;
      }
    }
  });
}
