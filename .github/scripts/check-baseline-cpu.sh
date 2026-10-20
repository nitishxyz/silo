#!/usr/bin/env bash
# Checks that x86_64 Linux builds run on CPUs without AVX-512 (or AVX at all
# in libghostty-vt): a runtime built for the build machine's CPU dies with
# SIGILL on older ones.
#
#   check-baseline-cpu.sh [binary...]
#
# 1. libghostty-vt.a (built for the baseline CPU) must contain no VEX/EVEX
#    vector code: no ymm/zmm or AVX-512 mask registers.
# 2. Each binary runs `--version` under qemu emulating a Haswell CPU (AVX2,
#    no AVX-512).
set -euo pipefail

[[ $(uname -sm) == "Linux x86_64" ]] || { echo "x86_64 Linux only" >&2; exit 1; }
root=$(cd "$(dirname "$0")/../.." && pwd)
revision=$(tr -d '[:space:]' < "$root/GHOSTTY_REV")
lib="$root/target/ghostty/$revision/runtime/lib/libghostty-vt.a"
[[ -f $lib ]] || { echo "missing $lib" >&2; exit 1; }
[[ $(cat "$root/target/ghostty/$revision/runtime/silo-cpu" 2>/dev/null) == baseline ]] ||
    { echo "libghostty-vt runtime is not tagged as built for the baseline CPU" >&2; exit 1; }

hits=$(objdump -d --no-show-raw-insn "$lib" | grep -E '%[yz]mm[0-9]|%k[0-7]\b' || true)
if [[ -n $hits ]]; then
    echo "libghostty-vt.a contains AVX/AVX-512 instructions:" >&2
    head -n 20 <<< "$hits" >&2
    exit 1
fi
echo "libghostty-vt.a: no AVX/AVX-512 instructions"

for bin in "$@"; do
    qemu-x86_64 -cpu Haswell "$bin" --version
    echo "$bin runs on Haswell (no AVX-512)"
done
