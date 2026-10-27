#!/usr/bin/env bash
# Wrap already-built debug binaries in a dev bundle CEF can start from:
#   <target>/debug/SiloDev.app  (exe = silo-app, helpers = copies of it,
#   framework = symlink into CEF_PATH, silo/silo-menubar/otto-silo symlinked
#   next to the exe, and the server the app runs: a copy of silo as
#   Contents/Helpers/SiloDev Server.app, as in Silo.app).
# Usage: scripts/dev-bundle.sh   (prints the bundle's executable path)
#
# The binaries must be dev-channel builds (plain `cargo build`, SILO_CHANNEL
# unset; crates/silo-channel): SiloDev then uses its own homes
# (~/Library/Application Support/silo-dev and otto-silo-dev), never the
# installed Silo's, whatever SILO_HOME it inherits.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
[[ "$target" = /* ]] || target="$root/$target"
out="$target/debug"
cef="${CEF_PATH:-$("$root/scripts/fetch-cef.sh" --print)}"
app="$out/SiloDev.app"
if [[ -x "$out/silo" ]]; then
  case "$("$out/silo" --version)" in
    *" dev") ;;
    *) echo "dev-bundle: $out/silo is not a dev-channel build (built with SILO_CHANNEL=release, or before channels); rebuild it with plain cargo build" >&2; exit 1 ;;
  esac
fi
framework="Chromium Embedded Framework.framework"

plist() { # exe id [helper] [icon]
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>$1</string>
  <key>CFBundleIdentifier</key><string>$2</string>
  <key>CFBundleName</key><string>$1</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Web pages in Silo can use your phone or a security key to sign in.</string>
  <key>NSCameraUsageDescription</key><string>Web pages in Silo can ask to use the camera.</string>
  <key>NSMicrophoneUsageDescription</key><string>Web pages in Silo can ask to use the microphone.</string>
  ${3:+<key>LSUIElement</key><true/>}
  ${4:+<key>CFBundleIconFile</key><string>$4</string><key>CFBundleIconName</key><string>$4</string>}
</dict></plist>
PLIST
}

# Copy to a fresh inode: macOS caches a Mach-O's signature per vnode, so
# overwriting an already-run binary in place gets it SIGKILLed at launch
# ("Code Signature Invalid").
install_bin() { # src dst
  cp -f "$1" "$2.tmp.$$"
  mv -f "$2.tmp.$$" "$2"
}
# Ad-hoc sign a bundle so Info.plist is bound (linker signatures aren't).
sign() {
  local log
  log=$(codesign --force --sign - --timestamp=none "$1" 2>&1) || { echo "$log" >&2; return 1; }
}

mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources"
icons=("$root/packaging/macos/Assets.car" "$root/packaging/macos/Silo.icns") # scripts/make-icons.sh
for f in "${icons[@]}"; do
  cmp -s "$f" "$app/Contents/Resources/$(basename "$f")" || cp "$f" "$app/Contents/Resources/"
done
plist SiloDev xyz.nitish.silo.dev "" Silo > "$app/Contents/Info.plist"
install_bin "$out/silo-app" "$app/Contents/MacOS/SiloDev"
for bin in silo silo-menubar otto-silo; do
  [[ -f "$out/$bin" ]] && ln -sf "$out/$bin" "$app/Contents/MacOS/$bin"
done
# The server's helper app (scripts/bundle-macos.sh): a real copy, as a
# symlinked executable resolves to target/debug/silo and runs without the
# bundle's identity. Copied and signed only when silo or the plist changed,
# so a running server's bundle stays put and its signature stable.
server_app="$app/Contents/Helpers/SiloDev Server.app"
if [[ -f "$out/silo" ]]; then
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
  info="$("$root/scripts/server-info-plist.sh" "SiloDev Server" xyz.nitish.silo.dev.server "$version" Silo)"
  stamp="$out/.silodev-server.stamp"
  want="$(stat -f '%m %z %i' "$out/silo") $(printf '%s' "$info" | shasum | cut -c1-40) $(cat "${icons[@]}" | shasum | cut -c1-40)"
  if [[ ! -x "$server_app/Contents/MacOS/silo" || "$(cat "$stamp" 2>/dev/null)" != "$want" ]]; then
    mkdir -p "$server_app/Contents/MacOS" "$server_app/Contents/Resources"
    printf '%s\n' "$info" > "$server_app/Contents/Info.plist"
    cp "${icons[@]}" "$server_app/Contents/Resources/"
    install_bin "$out/silo" "$server_app/Contents/MacOS/silo"
    sign "$server_app"
    echo "$want" > "$stamp"
  fi
fi
for suffix in "" " (GPU)" " (Renderer)" " (Plugin)"; do
  name="SiloDev Helper$suffix"
  dir="$app/Contents/Frameworks/$name.app/Contents"
  mkdir -p "$dir/MacOS"
  id="xyz.nitish.silo.dev.helper$(echo "$suffix" | tr -d ' ()' | tr 'A-Z' 'a-z')"
  plist "$name" "$id" 1 > "$dir/Info.plist"
  install_bin "$out/silo-app" "$dir/MacOS/$name"
  sign "$app/Contents/Frameworks/$name.app"
done
ln -sfn "$cef/$framework" "$app/Contents/Frameworks/$framework"
sign "$app"
echo "$app/Contents/MacOS/SiloDev"
