// Shared by build.rs (which precompiles the guest) and the runtime (which
// loads it). Codegen settings must match exactly or loading fails.
// Synchronous guest calls run on the caller's thread, so the wasm stack stays
// well under a 2 MiB thread stack. QuickJS's own stack check (on the shadow
// stack in linear memory) trips first, so deep recursion is a catchable error.
fn engine_config() -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    config.epoch_interruption(true);
    config.max_wasm_stack(1 << 20);
    config
}
