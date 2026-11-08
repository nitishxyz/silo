#!/usr/bin/env bash
# Values for corgi.toml's [env] probes. corgi runs these outside its build
# sandbox (from the workspace root) before building, and build scripts then
# see only what they print. The sandbox hides target/ and Cargo.lock from
# build scripts, so the native inputs that cargo builds keep under target/
# are staged into .corgi-inputs/ (gitignored) and handed over by a path
# relative to the package directory, which keeps the value the same in
# every checkout so the shared cache stays shared.
#
#   scripts/corgi-env.sh lock-sha256      SILO_CARGO_LOCK_SHA256 (silo-proto)
#   scripts/corgi-env.sh ghostty-runtime  SILO_GHOSTTY_RUNTIME (silo-vt)
#   scripts/corgi-env.sh needle-lib       SILO_NEEDLE_LIB (silo-voice)
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
staged="$root/.corgi-inputs"

sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }

# Replaces $2 with a copy of $1 (APFS clones where possible) unless the
# stamp file $3 already matches in both.
stage() {
    local from=$1 to=$2 stamp=$3
    if [[ -f $to/$stamp ]] && cmp -s "$from/$stamp" "$to/$stamp"; then
        return
    fi
    rm -rf "$to.tmp" "$to"
    mkdir -p "$(dirname "$to")"
    cp -Rc "$from" "$to.tmp" 2>/dev/null || cp -R "$from" "$to.tmp"
    # Debug info names the checkout (zig's lib dir, the ghostty source);
    # corgi rejects outputs carrying the workspace path, and the paths
    # would make every checkout's artifacts differ. Code is untouched.
    find "$to.tmp" -name '*.a' -exec strip -S {} +
    mv "$to.tmp" "$to"
}

case "${1:-}" in
    lock-sha256)
        sha256 "$root/Cargo.lock"
        ;;
    ghostty-runtime)
        # Builds (or reuses) target/ghostty/<rev>/runtime; zig and the
        # ghostty checkout need the network, which build scripts lack.
        runtime=$(bash "$root/scripts/build-ghostty.sh" | tail -n 1)
        stage "$runtime" "$staged/ghostty/runtime" silo-revision
        echo "../../.corgi-inputs/ghostty/runtime"
        ;;
    needle-lib)
        # The pin lives in silo-voice's build script; reuse it.
        build="$root/crates/silo-voice/build.rs"
        rev=$(sed -n 's/^const REV: &str = "\(.*\)";/\1/p' "$build")
        want=$(sed -n 's/^const SHA256: &str = "\(.*\)";/\1/p' "$build")
        lib="$staged/needle/$rev/libneedle.a"
        if [[ ! -f $lib || $(sha256 "$lib") != "$want" ]]; then
            cached="$root/target/needle/$rev/macos-arm64/libneedle.a"
            mkdir -p "$(dirname "$lib")"
            if [[ -f $cached && $(sha256 "$cached") == "$want" ]]; then
                cp -c "$cached" "$lib" 2>/dev/null || cp "$cached" "$lib"
            else
                curl -fsSL --retry 3 --proto =https --proto-redir =https -o "$lib.part" \
                    "https://huggingface.co/Cactus-Compute/needle3/resolve/$rev/macos-arm64/libneedle.a"
                [[ $(sha256 "$lib.part") == "$want" ]] || { echo "libneedle.a does not match the pin" >&2; exit 1; }
                mv "$lib.part" "$lib"
            fi
        fi
        echo "../../.corgi-inputs/needle/$rev/libneedle.a"
        ;;
    *)
        echo "usage: $0 lock-sha256|ghostty-runtime|needle-lib" >&2
        exit 2
        ;;
esac
