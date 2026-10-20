#!/bin/sh
# Notarize a signed .dmg or .zip: submit it, wait for the verdict and fail
# (printing Apple's log) unless it is Accepted.
# Usage: APPLE_ID=... APPLE_TEAM_ID=... APPLE_PASSWORD=... notarize.sh FILE
set -eu

file=$1
auth() {
  "$@" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD"
}
field() {
  printf '%s' "$out" | plutil -extract "$1" raw -o - - 2> /dev/null || true
}

rc=0
out=$(auth xcrun notarytool submit "$file" --wait --timeout 2h --output-format json) || rc=$?
printf '%s\n' "$out"
id=$(field id)
status=$(field status)
if [ "$rc" -ne 0 ] || [ "$status" != Accepted ]; then
  echo "notarizing $file failed (exit $rc, status '${status:-unknown}')" >&2
  if [ -n "$id" ]; then
    auth xcrun notarytool log "$id" >&2 || true
  fi
  exit 1
fi
echo "notarized $file ($id)"
