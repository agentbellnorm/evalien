//! Builds the guest for wasm32-unknown-unknown and precompiles it to native
//! code for the target, so the runtime ships no compiler and starts instantly.
//! A separate target dir keeps the nested cargo off the outer lock.

include!("src/engine_config.rs");
use std::{env, path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let target_dir = root.join("target/guest");
    println!("cargo:rerun-if-changed=../guest/src");
    println!("cargo:rerun-if-changed=../guest/Cargo.toml");
    println!("cargo:rerun-if-changed=src/engine_config.rs");

    let mut cargo = Command::new(env::var("CARGO").unwrap());
    cargo
        .current_dir(&root)
        .args([
            "build",
            "-p",
            "guest",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .arg("--target-dir")
        .arg(&target_dir);
    // Don't let the host build's settings leak into the guest build.
    for (key, _) in env::vars() {
        if key.starts_with("CARGO_") && key != "CARGO_HOME" || key == "RUSTFLAGS" || key == "TARGET" {
            cargo.env_remove(key);
        }
    }
    let status = cargo.status().expect("failed to run cargo for the guest");
    assert!(status.success(), "guest build failed");

    let wasm = std::fs::read(target_dir.join("wasm32-unknown-unknown/release/guest.wasm")).unwrap();
    let mut config = engine_config();
    config.target(&env::var("TARGET").unwrap()).unwrap();
    let engine = wasmtime::Engine::new(&config).unwrap();
    let native = engine.precompile_module(&wasm).expect("precompiling guest");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("guest.cwasm");
    std::fs::write(&out, native).unwrap();
    println!("cargo:rustc-env=GUEST_CWASM={}", out.display());
}
