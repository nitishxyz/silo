#!/usr/bin/env bash
# A release Silo-<version>.dmg built here, as release.yml's macOS job does:
# Silo.app (scripts/bundle-macos.sh) Developer ID signed under the hardened
# runtime, notarized with the app's bare `silo`, stapled, then the DMG around
# it signed, notarized, stapled and checked.
#
#   scripts/release-macos-local.sh
#
# SILO_SIGN_IDENTITY: the signing identity (default: the keychain's one
# "Developer ID Application" identity). NOTARY_PROFILE: a notarytool keychain
# profile (`xcrun notarytool store-credentials`; default "notary").
# Output: dist/Silo-<version>.dmg.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

profile="${NOTARY_PROFILE:-notary}"
if [[ -z "${SILO_SIGN_IDENTITY:-}" ]]; then
  SILO_SIGN_IDENTITY=$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)
fi
[[ -n "$SILO_SIGN_IDENTITY" ]] || { echo "no Developer ID Application identity in the keychain (set SILO_SIGN_IDENTITY)" >&2; exit 1; }
export SILO_SIGN_IDENTITY
xcrun notarytool history --keychain-profile "$profile" > /dev/null || { echo "notarytool profile '$profile' doesn't work (NOTARY_PROFILE)" >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
dmg="dist/Silo-$version.dmg"
echo "Silo $version, signed as: $SILO_SIGN_IDENTITY"

# Stapling and assessing look the ticket up online; right after notarization
# the connection can drop or the ticket not be found yet.
retry() {
  for attempt in 1 2 3 4 5 6; do
    if "$@"; then return; fi
    [[ "$attempt" -lt 6 ]] || return 1
    sleep $((attempt * 15))
  done
}
notarize() {
  local out id status
  out=$(xcrun notarytool submit "$1" --keychain-profile "$profile" --wait --timeout 2h --output-format json) || true
  echo "$out"
  id=$(printf '%s' "$out" | plutil -extract id raw -o - - 2> /dev/null || true)
  status=$(printf '%s' "$out" | plutil -extract status raw -o - - 2> /dev/null || true)
  if [[ "$status" != Accepted ]]; then
    echo "notarizing $1 failed (status '${status:-unknown}')" >&2
    [[ -n "$id" ]] && xcrun notarytool log "$id" --keychain-profile "$profile" >&2
    exit 1
  fi
}
staple() { retry xcrun stapler staple "$1"; }

scripts/bundle-macos.sh

bin=target/release/silo
case "$("$bin" --version)" in
  *" dev") echo "$bin is a dev-channel build" >&2; exit 1 ;;
esac
codesign --force --sign "$SILO_SIGN_IDENTITY" --options runtime --timestamp "$bin"
codesign --verify --strict "$bin"

# One submission tickets the bare `silo` and the app; the app is stapled so it passes Gatekeeper offline.
work=$(mktemp -d)
mkdir "$work/silo-notary"
ditto target/Silo.app "$work/silo-notary/Silo.app"
cp "$bin" "$work/silo-notary/silo"
ditto -c -k --keepParent "$work/silo-notary" "$work/silo-notary.zip"
notarize "$work/silo-notary.zip"
staple target/Silo.app

mkdir -p dist "$work/stage"
ditto target/Silo.app "$work/stage/Silo.app"
ln -s /Applications "$work/stage/Applications"
for attempt in 1 2 3 4 5; do
  if hdiutil create -volname Silo -srcfolder "$work/stage" -fs HFS+ -format UDZO -ov "$dmg"; then break; fi
  [[ "$attempt" -lt 5 ]] || exit 1
  sleep $((attempt * 5))
done
codesign --force --sign "$SILO_SIGN_IDENTITY" --timestamp "$dmg"
hdiutil verify "$dmg"
notarize "$dmg"
staple "$dmg"
retry xcrun stapler validate "$dmg"
spctl -a -t open --context context:primary-signature -vv "$dmg"

mnt=$(mktemp -d)
hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$dmg" > /dev/null
trap 'hdiutil detach "$mnt" > /dev/null 2>&1 || true' EXIT
codesign --verify --deep --strict "$mnt/Silo.app"
xcrun stapler validate "$mnt/Silo.app"
spctl -a -vv "$mnt/Silo.app"
server="Silo.app/Contents/Helpers/Silo Server.app"
codesign --verify --strict "$mnt/$server"
# Captured first: `grep -q` stops reading early, and codesign's SIGPIPE would fail the pipeline (pipefail).
identifier() { local info; info=$(codesign -dv "$1" 2>&1); grep -qx "Identifier=$2" <<< "$info"; }
identifier "$mnt/$server" xyz.nitish.silo.server
identifier "$mnt/Silo.app/Contents/Helpers/Silo Menu Bar.app" xyz.nitish.silo.menubar
echo "done: $dmg ($(du -h "$dmg" | cut -f1))"
