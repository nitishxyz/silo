#!/bin/sh
# Exercises site/install.sh against a fake release directory:
#   test-install.sh                 a stub `silo` (checks argument handling)
#   test-install.sh <release-dir>   real release assets (installs and runs
#                                   the platform's silo --version)
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
script=$root/site/install.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

case $(uname -s)-$(uname -m) in
    Linux-x86_64) asset=silo-linux-x86_64.tar.gz ;;
    Linux-aarch64) asset=silo-linux-aarch64.tar.gz ;;
    Darwin-arm64) asset=silo-macos-aarch64.tar.gz ;;
    *) echo "unsupported host" >&2; exit 1 ;;
esac

fail() { echo "FAIL: $*" >&2; exit 1; }

if [ $# -gt 0 ]; then
    release=$(cd "$1" && pwd)
    SILO_DOWNLOAD_BASE=file://$release SILO_INSTALL_DIR=$work/bin sh "$script" --no-start
    "$work/bin/silo" --version
    echo "ok: installed $asset from $release"
    exit 0
fi

# A stub silo that logs its arguments.
release=$work/release
mkdir -p "$release" "$work/stage"
cat > "$work/stage/silo" <<'EOF'
#!/bin/sh
if [ "${1:-}" = --version ]; then echo "silo 0.0.0-test"; exit 0; fi
echo "$*" >> "$SILO_TEST_LOG"
EOF
chmod 755 "$work/stage/silo"
tar -C "$work/stage" -czf "$release/$asset" silo
(cd "$release" && { sha256sum "$asset" 2>/dev/null || shasum -a 256 "$asset"; } > SHA256SUMS)

export SILO_TEST_LOG="$work/log"
run_script() { # the script on stdin, as `curl ... | env LC_TERMINAL= sh -s -- args`
    rm -rf "${work:?}/bin" "$SILO_TEST_LOG"
    : > "$SILO_TEST_LOG"
    SILO_DOWNLOAD_BASE=file://$release SILO_INSTALL_DIR=$work/bin env LC_TERMINAL= sh -s -- "$@" < "$script" 2> "$work/err" ||
        { cat "$work/err" >&2; return 1; }
}

run_script join 'silo1abc' || fail "join"
[ "$(cat "$SILO_TEST_LOG")" = "join silo1abc" ] || fail "join ran: $(cat "$SILO_TEST_LOG")"
[ -x "$work/bin/silo" ] || fail "not installed"
echo "ok: join <ticket> installs and runs only silo join"

run_script || fail "plain install"
[ "$(cat "$SILO_TEST_LOG")" = "up" ] || fail "plain install ran: $(cat "$SILO_TEST_LOG")"
echo "ok: plain install runs silo up"

run_script --version v9.9.9 --no-start || fail "--no-start"
[ ! -s "$SILO_TEST_LOG" ] || fail "--no-start ran: $(cat "$SILO_TEST_LOG")"
echo "ok: --no-start"

# The forms silo-cli builds (crates/silo-cli/src/link.rs): join_download and
# stage_release (into a directory that does not exist yet).
run_script --version 'v0.1.0' join 'silo1abc' || fail "--version join"
[ "$(cat "$SILO_TEST_LOG")" = "join silo1abc" ] || fail "--version join ran: $(cat "$SILO_TEST_LOG")"
echo "ok: --version v<ver> join <ticket>"
rm -rf "${work:?}/stage-dir"
: > "$SILO_TEST_LOG"
SILO_DOWNLOAD_BASE=file://$release SILO_INSTALL_DIR=$work/stage-dir/.silo.download sh -s -- --version 'v0.1.0' --no-start < "$script" 2> "$work/err" ||
    { cat "$work/err" >&2; fail "stage_release"; }
[ -x "$work/stage-dir/.silo.download/silo" ] || fail "stage_release: no binary"
[ ! -s "$SILO_TEST_LOG" ] || fail "stage_release ran: $(cat "$SILO_TEST_LOG")"
echo "ok: SILO_INSTALL_DIR=<new dir> --version v<ver> --no-start"

# Without SILO_DOWNLOAD_BASE: the GitHub release URLs (a fake curl serves
# them from $release by file name).
mkdir -p "$work/fakebin"
cat > "$work/fakebin/curl" <<'EOF'
#!/bin/sh
out=
while [ $# -gt 1 ]; do
    case $1 in -o) out=$2; shift ;; esac
    shift
done
echo "$1" >> "$SILO_TEST_URLS"
cp "$SILO_TEST_RELEASE/${1##*/}" "$out" 2> /dev/null || exit 22
EOF
chmod 755 "$work/fakebin/curl"
urls() { # version args...
    : > "$work/urls"
    SILO_TEST_URLS=$work/urls SILO_TEST_RELEASE=$release PATH=$work/fakebin:$PATH SILO_INSTALL_DIR=$work/bin \
        sh -s -- "$@" --no-start < "$script" 2> "$work/err" || { cat "$work/err" >&2; return 1; }
}
gh=https://github.com/nitishxyz/silo/releases
urls --version v0.1.0 || fail "default base"
[ "$(cat "$work/urls")" = "$gh/download/v0.1.0/$asset
$gh/download/v0.1.0/SHA256SUMS" ] || fail "fetched: $(cat "$work/urls")"
urls --version 0.1.0 || fail "bare version"
grep -q "^$gh/download/v0.1.0/SHA256SUMS\$" "$work/urls" || fail "bare version fetched: $(cat "$work/urls")"
(export SILO_VERSION=v0.1.0; urls) || fail "SILO_VERSION"
grep -q "^$gh/download/v0.1.0/$asset\$" "$work/urls" || fail "SILO_VERSION fetched: $(cat "$work/urls")"
urls || fail "latest"
grep -q "^$gh/latest/download/SHA256SUMS\$" "$work/urls" || fail "latest fetched: $(cat "$work/urls")"
echo "ok: release URLs (v<ver>, <ver>, SILO_VERSION, latest)"

if run_script join; then fail "join without a ticket succeeded"; fi
echo "ok: join without a ticket fails"

cp "$release/SHA256SUMS" "$work/sums"
echo "0000000000000000000000000000000000000000000000000000000000000000  $asset" > "$release/SHA256SUMS"
if run_script --no-start; then fail "bad checksum accepted"; fi
grep -q "checksum mismatch" "$work/err" || fail "bad checksum: $(cat "$work/err")"
echo "ok: checksum mismatch rejected"

echo "deadbeef  other.tar.gz" > "$release/SHA256SUMS"
if run_script --no-start; then fail "unlisted asset accepted"; fi
echo "ok: asset missing from SHA256SUMS rejected"
cp "$work/sums" "$release/SHA256SUMS"

echo "all install.sh tests passed"
