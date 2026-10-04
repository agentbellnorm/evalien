import { StringDecoder } from "node:string_decoder";
import type { OutputEvent } from "../contracts.mts";

/** Host output bypasses capture. Runtime writes still reach the actual terminal. */
export const writeStdout = process.stdout.write.bind(process.stdout);
export const writeStderr = process.stderr.write.bind(process.stderr);

type WriteCallback = (error?: Error | null) => void;

export function captureOutput(emit: (event: OutputEvent) => void): () => void {
  const restores = (["stdout", "stderr"] as const).map((type) => {
    const stream = process[type];
    const original = stream.write;
    const decoder = new StringDecoder("utf8");
    stream.write = function (
      chunk: string | Uint8Array,
      encodingOrCallback?: BufferEncoding | WriteCallback,
      callback?: WriteCallback,
    ): boolean {
      const encoding = typeof encodingOrCallback === "string" ? encodingOrCallback : undefined;
      const text = decoder.write(typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk);
      if (text) emit({ type, text });
      const done = typeof encodingOrCallback === "function" ? encodingOrCallback : callback;
      return original.call(this, chunk, encoding, done);
    };
    return () => {
      stream.write = original;
      const tail = decoder.end();
      if (tail) emit({ type, text: tail });
    };
  });
  return () => restores.forEach((restore) => restore());
}
