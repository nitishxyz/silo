#!/usr/bin/env bash
# Build libghostty-vt for wasm32-freestanding from the pinned ghostty source
# (scripts/build-ghostty.sh fetches it) into www/vt/ghostty-vt.wasm.
# Output and zig cache go under target/spikes; the source tree is untouched.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
rev=$(tr -d '[:space:]' <"$root/GHOSTTY_REV")
src="$root/target/ghostty/$rev/source"
zig=${ZIG:-$root/target/tools/zig-aarch64-macos-0.16.0/zig}
out="$root/target/spikes/ghostty-wasm"
[[ -d $src ]] || bash "$root/scripts/build-ghostty.sh" >/dev/null
(cd "$src" && "$zig" build -Demit-lib-vt=true -Dtarget=wasm32-freestanding -Doptimize=ReleaseSmall \
  -Dvt-features=-kitty-graphics --prefix "$out" --cache-dir "$root/target/spikes/zig-cache")
mkdir -p "$here/www/vt"
cp "$out/bin/ghostty-vt.wasm" "$here/www/vt/"
wasm="$here/www/vt/ghostty-vt.wasm"
printf 'ghostty-vt.wasm: %s bytes raw, %s gzip -9' "$(wc -c <"$wasm" | tr -d ' ')" "$(gzip -9c "$wasm" | wc -c | tr -d ' ')"
if command -v brotli >/dev/null; then printf ', %s brotli' "$(brotli -c -q 11 "$wasm" | wc -c | tr -d ' ')"; fi
echo
