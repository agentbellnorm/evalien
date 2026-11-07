// Installs the agent-facing globals and returns the bridge the Rust shim calls
// into. Everything here runs inside the sandbox, so the agent can tamper with
// it. That only hurts the agent: the host trusts nothing that comes out.
(raw, config) => {
  const indirectEval = eval;
  const { call, timerSet, timerClear, settled, output } = raw;
  const calls = new Map();
  const timers = new Map();
  let nextCall = 1;
  let nextTimer = 1;

  // Drop the frames that belong to the prelude, below the agent's code.
  const trim = (stack) => {
    const lines = String(stack).split("\n");
    const end = lines.findIndex((l) => l.includes("at eval (native)"));
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
  const report = (e) => (e instanceof Error ? inspect(e) : `Uncaught ${inspect(e)}`);

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
    timerSet(id, delay);
    return id;
  };
  const cancel = (id) => {
    if (timers.delete(id)) timerClear(id);
  };

  const log = (stream) => (...args) => output(stream, format(args));
  globalThis.console = { log: log(1), info: log(1), debug: log(1), warn: log(2), error: log(2) };
  globalThis.setTimeout = schedule(false);
  globalThis.setInterval = schedule(true);
  globalThis.clearTimeout = cancel;
  globalThis.clearInterval = cancel;
  globalThis.queueMicrotask ??= (fn) => void Promise.resolve().then(fn);

  // Each capability the host grants becomes an async global taking one
  // JSON-serializable argument. Nothing else from the outside exists.
  for (const name of config.capabilities) {
    globalThis[name] = (args) => hostCall(name, args ?? null);
  }
  if (config.capabilities.includes("fetch")) {
    const request = globalThis.fetch;
    globalThis.fetch = async (url, init = {}) => {
      const r = await request({ url: String(url), method: init.method, headers: init.headers, body: init.body, auth: init.auth });
      return {
        ok: r.status >= 200 && r.status < 300,
        status: r.status,
        headers: r.headers,
        text: async () => r.body,
        json: async () => JSON.parse(r.body),
      };
    };
  }

  return {
    evaluate(id, code) {
      let promise;
      try {
        // Same line as the code so error line numbers match what the agent wrote.
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
      const value = JSON.parse(payload);
      if (ok) pending.resolve(value);
      else pending.reject(new Error(String(value)));
    },
    fire(id) {
      const timer = timers.get(id);
      if (!timer) return;
      if (timer.repeat) timerSet(id, timer.delay);
      else timers.delete(id);
      try {
        timer.fn(...timer.args);
      } catch (e) {
        output(2, report(e));
      }
    },
  };
}
