import type { TrajectoryEvent } from "../contracts.mts";

/**
 * The first few model calls without human input run back to back, so the
 * agent can work. Further calls get further apart until input arrives.
 */
export const IDLE_DELAYS: readonly number[] = [0, 0, 0, 5_000, 30_000, 120_000, 600_000];

/**
 * Whether an observation asks for a model call. Results and input always do.
 * Output does when nothing is pending; a pending evaluation's output is seen
 * with its result. Submitted source and model calls never do.
 */
export function wakes(event: TrajectoryEvent, pending: number): boolean {
  switch (event.type) {
    case "start":
    case "stdin":
    case "result":
      return true;
    case "stdout":
    case "stderr":
      return pending === 0;
    case "eval":
    case "generation":
      return false;
    default: {
      const exhaustive: never = event;
      throw new TypeError(`Unknown trajectory event: ${String(exhaustive)}`);
    }
  }
}

/** A fresh start or human input resets pacing. */
export function fromHuman(event: TrajectoryEvent): boolean {
  switch (event.type) {
    case "start":
    case "stdin":
      return true;
    case "eval":
    case "stdout":
    case "stderr":
    case "result":
    case "generation":
      return false;
    default: {
      const exhaustive: never = event;
      throw new TypeError(`Unknown trajectory event: ${String(exhaustive)}`);
    }
  }
}

/** Minimum spacing before the nth consecutive call without human input. */
export function idleDelay(quiet: number, delays: readonly number[] = IDLE_DELAYS): number {
  if (quiet <= 0 || delays.length === 0) return 0;
  return delays[Math.min(quiet, delays.length) - 1];
}
