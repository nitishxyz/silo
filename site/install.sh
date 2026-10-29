#!/bin/sh
# Install silo and start its server.
#
#   curl -fsSL https://nitishxyz.github.io/silo/install.sh | sh
#   curl -fsSL https://nitishxyz.github.io/silo/install.sh | sh -s -- join <ticket>
#   curl -fsSL https://nitishxyz.github.io/silo/install.sh | sh -s -- --version v0.1.0
#
# Downloads silo-<os>-<arch>.tar.gz from the GitHub release, checks it
# against the release's SHA256SUMS and installs silo to ~/.local/bin.
#
# Options (before any silo arguments):
#   --version <tag>   install that release (v0.1.0 or 0.1.0) instead of the
#                     latest (or SILO_VERSION)
#   --no-start        install only; do not run silo afterwards
# Then runs `silo up`, followed by `silo <args>` if arguments remain. With
# `join <ticket>` or `up ...` only that command runs (it starts the server).
#
# Environment:
#   SILO_VERSION        release tag to install (default: latest)
#   SILO_INSTALL_DIR    install directory (default: ~/.local/bin)
#   SILO_DOWNLOAD_BASE  directory URL holding the release assets; overrides
#                       the GitHub Releases location (for mirrors and tests)
#
# Everything runs from main, called on the last line, so a truncated
# download does nothing.
set -eu

repo=nitishxyz/silo
glibc_min=2.35

say() { printf 'silo: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

# Fails unless the C library is glibc >= $glibc_min.
check_glibc() {
    libc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
    if [ -z "$libc" ]; then
        libc=$(ldd --version 2>&1 | head -n 1 || true)
        case $libc in
            *musl* | *MUSL*) die "this system uses musl; silo needs glibc $glibc_min or newer" ;;
        esac
    fi
    have=$(printf '%s\n' "$libc" | grep -Eo '[0-9]+\.[0-9]+' | tail -n 1 || true)
    [ -n "$have" ] || die "cannot determine the glibc version; silo needs glibc $glibc_min or newer"
    if [ "${have%%.*}" -lt "${glibc_min%%.*}" ] ||
        { [ "${have%%.*}" -eq "${glibc_min%%.*}" ] && [ "${have#*.}" -lt "${glibc_min#*.}" ]; }; then
        die "glibc $have is too old; silo needs glibc $glibc_min or newer"
    fi
}

main() {
    version=${SILO_VERSION:-}
    start=1
    while [ $# -gt 0 ]; do
        case $1 in
            --version) [ $# -ge 2 ] || die "--version needs a tag"; version=$2; shift 2 ;;
            --version=*) version=${1#--version=}; shift ;;
            --no-start) start=; shift ;;
            --) shift; break ;;
            *) break ;;
        esac
    done
    if [ "${1:-}" = join ] && [ -z "${2:-}" ]; then
        die "usage: sh -s -- join <ticket>"
    fi
    case $version in
        [0-9]*) version=v$version ;;
    esac

    case $(uname -s) in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) die "unsupported OS: $(uname -s)" ;;
    esac
    arch=$(uname -m)
    case $arch in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    if [ "$os" = macos ] && [ "$arch" = x86_64 ]; then
        # A Rosetta shell on Apple silicon reports x86_64.
        if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
            arch=aarch64
        else
            die "Intel Macs are not supported"
        fi
    fi
    if [ "$os" = linux ]; then
        check_glibc
    fi

    asset=silo-$os-$arch.tar.gz
    if [ -n "${SILO_DOWNLOAD_BASE:-}" ]; then
        base=${SILO_DOWNLOAD_BASE%/}
    elif [ -n "$version" ]; then
        base=https://github.com/$repo/releases/download/$version
    else
        base=https://github.com/$repo/releases/latest/download
    fi

    # The archive shows a progress bar on a terminal (also under
    # `silo remote install`, whose ssh has one); small files never do.
    if command -v curl > /dev/null 2>&1; then
        fetch() { curl -fsL --retry 3 -o "$2" "$1"; }
        if [ -t 2 ]; then
            fetch_shown() { curl -fL --progress-bar --retry 3 -o "$2" "$1"; }
        else
            fetch_shown() { fetch "$@"; }
        fi
    elif command -v wget > /dev/null 2>&1; then
        fetch() { wget -q -O "$2" "$1"; }
        fetch_shown() { fetch "$@"; }
    else
        die "need curl or wget"
    fi
    if command -v sha256sum > /dev/null 2>&1; then
        sha256() { sha256sum "$1" | cut -d' ' -f1; }
    elif command -v shasum > /dev/null 2>&1; then
        sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
    else
        die "need sha256sum or shasum"
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t silo)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' HUP INT TERM

    say "downloading ${version:-latest} for $os-$arch"
    fetch_shown "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
    expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1; exit }' "$tmp/SHA256SUMS")
    [ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
    actual=$(sha256 "$tmp/$asset")
    [ "$expected" = "$actual" ] || die "checksum mismatch for $asset"

    tar -xzf "$tmp/$asset" -C "$tmp" silo || die "bad archive: $asset"

    dir=${SILO_INSTALL_DIR:-$HOME/.local/bin}
    mkdir -p "$dir"
    # Rename into place so a running silo keeps its old binary.
    cp "$tmp/silo" "$dir/.silo.new.$$"
    chmod 755 "$dir/.silo.new.$$"
    mv -f "$dir/.silo.new.$$" "$dir/silo"
    silo=$dir/silo
    say "installed $("$silo" --version 2>/dev/null || echo silo) to $silo"

    case :$PATH: in
        *:"$dir":*) ;;
        *)
            rc=$HOME/.profile
            case ${SHELL:-} in
                */zsh) rc=$HOME/.zshrc ;;
                */bash) if [ "$os" = macos ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi ;;
            esac
            say "$dir is not on your PATH; add it with:"
            # shellcheck disable=SC2016 # $PATH is meant literally
            printf '\n    echo '\''export PATH="%s:$PATH"'\'' >> %s\n\n' "$dir" "$rc" >&2
            ;;
    esac

    [ -n "$start" ] || return 0
    say "starting the server"

    # `join` and `up` start the server themselves.
    case ${1:-} in
        join | up) ;;
        *) run up ;;
    esac
    if [ $# -gt 0 ]; then
        run "$@"
    fi
}

# Under `curl | sh` stdin is the script; give silo the terminal instead.
run() {
    if [ ! -t 0 ] && (: < /dev/tty) 2>/dev/null; then
        "$silo" "$@" < /dev/tty
    else
        "$silo" "$@"
    fi
}

main "$@"
