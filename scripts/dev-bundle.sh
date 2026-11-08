#!/usr/bin/env bash
# Wrap already-built debug binaries in a dev bundle CEF can start from:
#   <target>/debug/SiloDev.app  (exe = silo-app, helpers = copies of it,
#   framework = symlink into CEF_PATH, silo symlinked next to the exe, and
#   as in Silo.app the server bundle the app runs, with the menu-bar agent
#   inside it: Contents/Helpers/SiloDev Server.app/Contents/Helpers/SiloDev Menu Bar.app).
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
[[ -f "$out/silo" ]] && ln -sf "$out/silo" "$app/Contents/MacOS/silo"
# Earlier layouts: otto (now `silo otto`), the agent beside the app or as a
# helper of its own (now inside the server bundle).
rm -rf "$app/Contents/MacOS/silo-menubar" "$app/Contents/MacOS/otto-silo" "$app/Contents/Helpers/SiloDev Menu Bar.app"
# The server bundle (scripts/bundle-macos.sh): real copies, as a symlinked
# executable resolves to target/debug and runs without the bundle's
# identity. Copied and signed only when a binary or plist changed, so a
# running server's or agent's bundle stays put and its signature stable.
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
helper_app() { # bin bundle-path name id plist-script stamp [extra-stamp]
  local bundle="$2" info want
  [[ -f "$out/$1" ]] || return 0
  info="$("$root/scripts/$5" "$3" "$4" "$version" Silo)"
  want="$(stat -f '%m %z %i' "$out/$1") $(printf '%s' "$info" | shasum | cut -c1-40) $(cat "${icons[@]}" | shasum | cut -c1-40) ${7:-}"
  if [[ ! -x "$bundle/Contents/MacOS/$1" || "$(cat "$out/$6" 2>/dev/null)" != "$want" ]]; then
    mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
    printf '%s\n' "$info" > "$bundle/Contents/Info.plist"
    cp "${icons[@]}" "$bundle/Contents/Resources/"
    install_bin "$out/$1" "$bundle/Contents/MacOS/$1"
    sign "$bundle"
    echo "$want" > "$out/$6"
  fi
}
server="$app/Contents/Helpers/SiloDev Server.app"
# Inside out: the agent first; a new agent re-signs the server bundle.
helper_app silo-menubar "$server/Contents/Helpers/SiloDev Menu Bar.app" "SiloDev Menu Bar" xyz.nitish.silo.dev.menubar menubar-info-plist.sh .silodev-menubar.stamp
helper_app silo "$server" "SiloDev Server" xyz.nitish.silo.dev.server server-info-plist.sh .silodev-server.stamp "$(cat "$out/.silodev-menubar.stamp" 2>/dev/null | shasum | cut -c1-40)"
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
