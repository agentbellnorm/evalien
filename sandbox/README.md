# sandbox

A spike of the evaluator for a Rust rewrite of evalien. The agent's JavaScript runs in QuickJS compiled to WebAssembly, inside wasmtime, inside a single small binary. It can't see the process, files, env, network or API key, because none of those exist in its world. Anything it can do is a capability the host grants, and every call to one goes through the host.

The host owns the event loop, the same way evalien does. `eval` returns at the first `await`. Output, settled results and resets come out of `next_event` as they happen, so evals run concurrently.

## Pieces

- **guest/**: rquickjs built for `wasm32-unknown-unknown`, with no WASI. Its only imports are the six bridge functions plus a clock. `prelude.js` installs `console`, timers and one async global per granted capability. It returns the bridge the shim calls into.
- **host/**: wasmtime with no compiler at runtime. `build.rs` builds the guest and precompiles it for the target. `Sandbox` runs every synchronous slice of guest code under CPU and memory limits, then turns what the guest asked for into tokio tasks.
- **host/src/capabilities.rs**: the one choke point for everything outside the sandbox. This is where policy, auditing and secrets-by-reference go.

## Limits

| | default | what happens |
| --- | --- | --- |
| Soft slice | 1s | QuickJS throws an uncatchable `interrupted`, the eval fails, and globals survive |
| Hard slice | 3s | wasmtime's epoch deadline traps, the instance is replaced, pending evals fail, and globals are lost |
| Memory | 64 MiB | QuickJS gets 75% as its heap, so OOM is normally a catchable JS error |
| Recursion | 128 KiB JS stack | catchable error, with wasm's 1 MiB stack as the backstop |
| Output | 1 MiB per slice, 64 KiB per chunk | truncated with a notice; results always get through |

## Numbers (M-series Mac, release)

- 4.2 MB binary, 1.8 MB gzipped. It depends only on libSystem.
- About 4 ms from process start to a ready runtime.
- Clean release build about 2 min. Tests run in 0.3 s.

## Running

```
./scripts/setup.sh      # wasi-sdk into .tools/ (clang for QuickJS → wasm)
cargo test -p host
cargo run -p host       # REPL: one eval per line, with echo() and delay() granted
```

## Known gaps

- An interrupt that hits **after an `await`** escapes from a promise job, and nothing ties it back to its eval. It's reported on stderr, but the eval stays pending. A fix needs either a small QuickJS patch, so the interrupt rejects the running async function, or tracking the current eval through the prelude.
- No fuel metering yet. CPU is bounded by wall-clock per slice, not by total CPU per eval.
- No TypeScript stripping (oxc) yet.
- No real capabilities (fetch, fs, db) or policy layer yet.
- Linux is untested. Nothing here is macOS-specific, but CI should prove that.
