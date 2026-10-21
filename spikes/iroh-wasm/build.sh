#!/usr/bin/env bash
# Build the spike for the browser into www/pkg (gitignored).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/spikes}"
# rustup's toolchain carries the wasm32 std; Homebrew's rustc doesn't.
export PATH="$HOME/.cargo/bin:$CARGO_TARGET_DIR/bin/bin:$PATH"
# ring compiles C for wasm32; Apple clang can do it, Homebrew llvm too.
if [[ -x /opt/homebrew/opt/llvm/bin/clang ]]; then
  export CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang
  export AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar
fi
cd "$here"
cargo build --lib --release --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir www/pkg \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/silo_iroh_wasm_spike.wasm"
wasm=www/pkg/silo_iroh_wasm_spike_bg.wasm
printf 'wasm: %s bytes raw, %s gzip -9' "$(wc -c <"$wasm" | tr -d ' ')" "$(gzip -9c "$wasm" | wc -c | tr -d ' ')"
if command -v brotli >/dev/null; then printf ', %s brotli' "$(brotli -c -q 11 "$wasm" | wc -c | tr -d ' ')"; fi
echo
