#!/bin/sh
# Fetches wasi-sdk into .tools/: a wasm-capable clang to compile QuickJS for
# the guest. The Rust toolchain itself comes from rust-toolchain.toml.
set -eu
VERSION=34
cd "$(dirname "$0")/.."
[ -x .tools/wasi-sdk/bin/clang ] && { echo "wasi-sdk already in .tools/"; exit 0; }

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) PLATFORM=arm64-macos ;;
  Darwin-x86_64) PLATFORM=x86_64-macos ;;
  Linux-x86_64) PLATFORM=x86_64-linux ;;
  Linux-aarch64) PLATFORM=arm64-linux ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

NAME="wasi-sdk-$VERSION.0-$PLATFORM"
mkdir -p .tools
curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$VERSION/$NAME.tar.gz" | tar xz -C .tools
mv ".tools/$NAME" .tools/wasi-sdk
echo "wasi-sdk $VERSION installed in .tools/wasi-sdk"
