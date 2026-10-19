#!/usr/bin/env bash
# Wrap already-built debug binaries in a dev bundle CEF can start from:
#   <target>/debug/SiloDev.app  (exe = silo-app, helpers = copies of it,
#   framework = symlink into CEF_PATH, silo/silo-menubar next to the exe).
# Usage: scripts/dev-bundle.sh   (prints the bundle's executable path)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
[[ "$target" = /* ]] || target="$root/$target"
out="$target/debug"
cef="${CEF_PATH:-$("$root/scripts/fetch-cef.sh" --print)}"
app="$out/SiloDev.app"
framework="Chromium Embedded Framework.framework"

plist() { # exe id [helper]
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>$1</string>
  <key>CFBundleIdentifier</key><string>$2</string>
  <key>CFBundleName</key><string>$1</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  ${3:+<key>LSUIElement</key><true/>}
</dict></plist>
PLIST
}

mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks"
plist SiloDev xyz.nitish.silo.dev > "$app/Contents/Info.plist"
cp -f "$out/silo-app" "$app/Contents/MacOS/SiloDev"
for bin in silo silo-menubar; do
  [[ -f "$out/$bin" ]] && ln -sf "$out/$bin" "$app/Contents/MacOS/$bin"
done
for suffix in "" " (GPU)" " (Renderer)" " (Plugin)"; do
  name="SiloDev Helper$suffix"
  dir="$app/Contents/Frameworks/$name.app/Contents"
  mkdir -p "$dir/MacOS"
  id="xyz.nitish.silo.dev.helper$(echo "$suffix" | tr -d ' ()' | tr 'A-Z' 'a-z')"
  plist "$name" "$id" 1 > "$dir/Info.plist"
  cp -f "$out/silo-app" "$dir/MacOS/$name"
done
ln -sfn "$cef/$framework" "$app/Contents/Frameworks/$framework"
echo "$app/Contents/MacOS/SiloDev"
