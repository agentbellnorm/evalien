//! Builds the JavaScript guest (guest/) as a WebAssembly component and embeds
//! it. A separate target dir keeps the nested cargo off the outer build lock.

use std::{env, path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let target_dir = root.join("target/guest");
    for path in ["guest/src", "guest/Cargo.toml", "wit"] {
        println!("cargo:rerun-if-changed={path}");
    }

    let mut cargo = Command::new(env::var("CARGO").expect("cargo sets CARGO"));
    cargo
        .current_dir(&root)
        .args([
            "build",
            "--package",
            "guest",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--target-dir",
        ])
        .arg(&target_dir);
    // The host build's settings must not leak into the guest build.
    for (key, _) in env::vars() {
        if key.starts_with("CARGO_") && key != "CARGO_HOME" || key == "RUSTFLAGS" || key == "TARGET" {
            cargo.env_remove(key);
        }
    }
    let status = cargo.status().expect("failed to run cargo for the guest");
    assert!(
        status.success(),
        "building the guest failed; is wasi-sdk installed? see scripts/setup.sh"
    );

    let component = target_dir.join("wasm32-wasip2/release/guest.wasm");
    println!("cargo:rustc-env=GUEST_COMPONENT={}", component.display());
}
