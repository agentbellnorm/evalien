// Installs the agent-facing globals and returns the bridge the Rust side calls
// into. This runs inside the sandbox, so the agent can tamper with it. That
// only hurts the agent: the host trusts nothing that comes out.
(raw, capabilities) => {
  const indirectEval = eval;
  const { call, setTimer, clearTimer, settled, output } = raw;
  const calls = new Map();
  const timers = new Map();
  let nextCall = 1;
  let nextTimer = 1;

  // Stack frames below the agent's code belong to the prelude.
  const trim = (stack) => {
    const lines = String(stack).split("\n");
    const end = lines.findIndex((line) => line.includes("at eval (native)"));
    return (end === -1 ? lines : lines.slice(0, end)).join("\n").trimEnd();
  };
  const describeError = (e) => (e.stack ? `${e.name}: ${e.message}\n${trim(e.stack)}` : `${e.name}: ${e.message}`);

  const inspect = (value, seen = new Set()) => {
    switch (typeof value) {
      case "undefined": return "undefined";
      case "string": return JSON.stringify(value);
      case "bigint": return `${value}n`;
      case "symbol": return value.toString();
      case "function": return `[Function ${value.name || "(anonymous)"}]`;
      case "number": case "boolean": return String(value);
    }
    if (value === null) return "null";
    if (value instanceof Error) return describeError(value);
    if (seen.has(value)) return "[Circular]";
    seen.add(value);
    try {
      if (Array.isArray(value)) return `[${value.map((v) => inspect(v, seen)).join(", ")}]`;
      if (value instanceof Map) return `Map(${value.size}) {${[...value].map(([k, v]) => `${inspect(k, seen)} => ${inspect(v, seen)}`).join(", ")}}`;
      if (value instanceof Set) return `Set(${value.size}) {${[...value].map((v) => inspect(v, seen)).join(", ")}}`;
      if (value instanceof Date) return value.toISOString();
      if (value instanceof Promise) return "Promise {}";
      const entries = Object.entries(value).map(([k, v]) => `${/^[A-Za-z_$][\w$]*$/.test(k) ? k : JSON.stringify(k)}: ${inspect(v, seen)}`);
      const name = value.constructor && value.constructor !== Object ? `${value.constructor.name} ` : "";
      return `${name}{${entries.join(", ")}}`;
    } finally {
      seen.delete(value);
    }
  };

  const format = (args) => args.map((a) => (typeof a === "string" ? a : inspect(a))).join(" ");
  const report = (e) => (e instanceof Error ? describeError(e) : `Uncaught ${inspect(e)}`);

  const hostCall = (name, args) =>
    new Promise((resolve, reject) => {
      const id = nextCall++;
      calls.set(id, { resolve, reject });
      call(id, name, JSON.stringify(args));
    });

  const schedule = (repeat) => (fn, ms = 0, ...args) => {
    if (typeof fn !== "function") throw new TypeError("callback must be a function");
    const id = nextTimer++;
    const delay = Math.max(0, Number(ms) || 0);
    timers.set(id, { fn, args, repeat, delay });
    setTimer(id, delay);
    return id;
  };
  const cancel = (id) => {
    if (timers.delete(id)) clearTimer(id);
  };

  const log = (stderr) => (...args) => output(stderr, format(args));
  globalThis.console = { log: log(false), info: log(false), debug: log(false), warn: log(true), error: log(true) };
  globalThis.setTimeout = schedule(false);
  globalThis.setInterval = schedule(true);
  globalThis.clearTimeout = cancel;
  globalThis.clearInterval = cancel;
  globalThis.queueMicrotask ??= (fn) => void Promise.resolve().then(fn);

  // Each granted capability gets its JavaScript shape. Nothing else from the
  // outside exists.
  for (const name of capabilities) {
    switch (name) {
      case "sql":
        globalThis.db = Object.freeze({
          query: (sql, ...params) => hostCall("sql", { sql: String(sql), params, mode: "query" }),
          run: (sql, ...params) => hostCall("sql", { sql: String(sql), params, mode: "run" }),
        });
        break;
      case "connections":
        globalThis.connections = Object.freeze({
          list: () => hostCall("connections", { op: "list" }),
          skill: (name) => hostCall("connections", { op: "skill", name: String(name) }),
          connect: (name, { timeout = 180_000 } = {}) => hostCall("connections", { op: "connect", name: String(name), timeout }),
        });
        break;
      case "fetch":
        globalThis.fetch = async (url, init = {}) => {
          const response = await hostCall("fetch", {
            url: String(url),
            method: init.method ?? "GET",
            headers: init.headers ?? {},
            body: init.body ?? null,
          });
          return {
            ok: response.status >= 200 && response.status < 300,
            status: response.status,
            url: response.url,
            headers: response.headers,
            text: async () => response.body,
            json: async () => JSON.parse(response.body),
          };
        };
        break;
      default:
        globalThis[name] = (args) => hostCall(name, args ?? null);
    }
  }

  return {
    evaluate(id, code) {
      let promise;
      try {
        // Same line as the code, so error line numbers match what the agent wrote.
        promise = indirectEval(`(async () => {${code}\n})()`);
      } catch (e) {
        settled(id, false, report(e));
        return;
      }
      promise.then(
        (value) => settled(id, true, inspect(value)),
        (e) => settled(id, false, report(e)),
      );
    },
    complete(id, ok, payload) {
      const pending = calls.get(id);
      if (!pending) return;
      calls.delete(id);
      if (ok) {
        pending.resolve(JSON.parse(payload));
      } else {
        // The host's message is the whole story; a stack here would only show the prelude.
        const error = new Error(payload);
        delete error.stack;
        pending.reject(error);
      }
    },
    fire(id) {
      const timer = timers.get(id);
      if (!timer) return;
      if (timer.repeat) setTimer(id, timer.delay);
      else timers.delete(id);
      try {
        timer.fn(...timer.args);
      } catch (e) {
        output(true, report(e));
      }
    },
  };
}
