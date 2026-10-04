// Bound before anything captures the streams, so lifecycle messages never
// become observations.
const writeStderr = process.stderr.write.bind(process.stderr);

/** Take the environment and clear it, so evaluated code never sees credentials. */
export function takeEnvironment(): Record<string, string | undefined> {
  const env = { ...process.env };
  for (const key of Object.keys(process.env)) delete process.env[key];
  return env;
}

type ErrorClass = abstract new (...args: never[]) => Error;

/**
 * Run until `main` settles or a stop signal arrives, then clean up and exit.
 * Errors listed in `exitCodes` are expected stops: their message is printed
 * and the process exits with that code. Anything else is rethrown.
 *
 * As PID 1 in a container, the process has no default signal handling, so
 * Ctrl+C and `podman stop` only work through these handlers. The explicit
 * exit also ends timers that evaluated code left running.
 */
export async function runUntilExit(
  main: () => Promise<void>,
  { cleanup, exitCodes = [] }: { cleanup: () => void; exitCodes?: [ErrorClass, number][] },
): Promise<never> {
  let cleaned = false;
  const once = () => {
    if (cleaned) return;
    cleaned = true;
    cleanup();
  };
  for (const [signal, code] of [["SIGINT", 130], ["SIGTERM", 143]] as const) {
    process.on(signal, () => {
      writeStderr(`\n${signal}: stopping.\n`);
      once();
      process.exit(code);
    });
  }
  try {
    await main();
    once();
    process.exit(0);
  } catch (err) {
    const expected = exitCodes.find(([type]) => err instanceof type);
    once();
    if (!expected) throw err;
    writeStderr(`\n${(err as Error).message}. Stopping.\n`);
    process.exit(expected[1]);
  }
}
