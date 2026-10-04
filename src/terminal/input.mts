import readline from "node:readline";
import type { Readable } from "node:stream";
import type { InputSource } from "../contracts.mts";

/** Human input, one nonblank line at a time. Closing the stream ends input only. */
export function lineInput(stream: Readable = process.stdin): InputSource {
  return (onLine) => {
    const rl = readline.createInterface({ input: stream, prompt: "" });
    rl.on("line", (line: string) => {
      if (line.trim()) onLine(line);
    });
    return () => rl.close();
  };
}
