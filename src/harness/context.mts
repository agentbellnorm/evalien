import type { Entry, EventLog } from "../contracts.mts";

// How the trajectory becomes prompt text.

/** Larger bodies keep their head and tail; the full event stays in the trajectory. */
export const MAX_ENTRY_CHARS = 8_000;

function clip(seq: number, body: string): string {
  if (body.length <= MAX_ENTRY_CHARS) return body;
  const half = MAX_ENTRY_CHARS / 2;
  return `${body.slice(0, half)}\n… ${body.length - MAX_ENTRY_CHARS} characters omitted; full text in trajectory seq ${seq} …\n${body.slice(-half)}`;
}

/** Each entry renders independently, so appending preserves the existing text. */
export function renderEntry({ seq, timestamp, event }: Entry): string {
  let label: string = event.type;
  let body: string;
  switch (event.type) {
    case "start":
      body = "Fresh runtime. Previous live bindings, callbacks, and connections are gone.";
      break;
    case "eval":
      body = event.code;
      break;
    case "stdin":
    case "stdout":
    case "stderr":
      body = event.text;
      break;
    case "result":
      label = `result eval=${event.evalId} ${event.outcome}`;
      body = event.text;
      break;
    default: {
      const exhaustive: never = event;
      throw new TypeError(`Unknown trajectory event: ${exhaustive}`);
    }
  }
  return `[${seq} ${timestamp} ${label}]\n${clip(seq, body).split("\n").map((line) => `| ${line}`).join("\n")}\n\n`;
}

export function renderTrajectory(entries: readonly Entry[]): string {
  return entries.map(renderEntry).join("");
}

/** Bounds input cost per call, even when evaluated code floods output. */
export const MAX_PROMPT_CHARS = 400_000;

/**
 * A recent window whose start moves in fixed steps, so consecutive prompts
 * share a prefix. Only an oversized window drops older entries beyond that.
 */
export function promptWindow(log: EventLog, through: number): string[] {
  let after = Math.floor(Math.max(0, through - 500) / 200) * 200;
  let blocks = log.read(after, through).map(renderEntry);
  const size = () => blocks.reduce((total, block) => total + block.length, 0);
  while (after + 200 < through && size() > MAX_PROMPT_CHARS) {
    after += 200;
    blocks = log.read(after, through).map(renderEntry);
  }
  while (blocks.length > 1 && size() > MAX_PROMPT_CHARS) blocks.shift();
  return blocks;
}
