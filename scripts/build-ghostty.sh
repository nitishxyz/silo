#!/usr/bin/env bash
# Builds the pinned libghostty-vt static library into target/ghostty/<rev>/runtime.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
revision=$(tr -d '[:space:]' < "$root/GHOSTTY_REV")
zig_version=0.16.0
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) zig_target=aarch64-macos ;;
    Darwin-x86_64) zig_target=x86_64-macos ;;
    Linux-x86_64) zig_target=x86_64-linux ;;
    Linux-aarch64) zig_target=aarch64-linux ;;
    *) echo "unsupported host" >&2; exit 1 ;;
esac
tools="$root/target/tools"
zig=${ZIG:-$tools/zig-$zig_target-$zig_version/zig}
if [[ ! -x $zig ]]; then
    mkdir -p "$tools"
    curl -sSfL "https://ziglang.org/download/$zig_version/zig-$zig_target-$zig_version.tar.xz" | tar xJ -C "$tools"
fi
[[ $("$zig" version) == "$zig_version" ]] || { echo "need zig $zig_version" >&2; exit 1; }

cache="$root/target/ghostty/$revision"
runtime="$cache/runtime"
if [[ -f $runtime/silo-revision && $(cat "$runtime/silo-revision") == "$revision" ]]; then
    echo "$runtime"
    exit 0
fi
mkdir -p "$cache"
if [[ ! -d $cache/source/.git ]]; then
    git init -q "$cache/source"
    git -C "$cache/source" fetch -q --depth 1 https://github.com/ghostty-org/ghostty "$revision"
    git -C "$cache/source" checkout -q --detach FETCH_HEAD
fi
(
    cd "$cache/source"
    "$zig" build -Demit-lib-vt=true -Doptimize=ReleaseFast -Dsimd=false \
        -Dvt-features=-kitty-graphics --prefix "$runtime" >&2
)
printf '%s\n' "$revision" > "$runtime/silo-revision"
echo "$runtime"
