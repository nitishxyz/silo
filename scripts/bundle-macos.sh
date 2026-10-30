#!/usr/bin/env bash
# Build target/Silo.app: the GPUI client and the otto-silo daemon (agent
# blocks) in Contents/MacOS, and two helper apps in Contents/Helpers: the
# `silo` server/CLI binary as Silo Server.app (the identity macOS privacy
# prompts show for programs in Silo's terminals) and the silo-menubar agent
# as Silo Menu Bar.app (its own bundle id, so it is never taken for Silo).
#
# Signed ad-hoc, or with SILO_SIGN_IDENTITY (a Developer ID Application
# identity in the keychain) under the hardened runtime with secure
# timestamps, ready for notarization (.github/workflows/release.yml).
#
#   scripts/bundle-macos.sh            # release build
#   PROFILE=debug scripts/bundle-macos.sh
#   SILO_APP_FEATURES="" scripts/bundle-macos.sh   # precompiled Metal shaders (needs Xcode Metal toolchain)
#
# Always a release-channel build (SILO_CHANNEL=release, crates/silo-channel):
# Silo.app uses the installed app's homes (~/Library/Application Support/silo
# and otto-silo), launchd job and bundle id. For a development app use
# scripts/dev-bundle.sh (SiloDev.app, dev channel: …/silo-dev, otto-silo-dev).
set -euo pipefail

export SILO_CHANNEL=release

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

profile="${PROFILE:-release}"
features="${SILO_APP_FEATURES-runtime_shaders}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

cargo_flags=()
[[ "$profile" == release ]] && cargo_flags+=(--release)
app_flags=(--no-default-features)
[[ -n "$features" ]] && app_flags+=(--features "$features")

cargo build "${cargo_flags[@]}" -p silo-cli -p silo-menubar -p otto-cli --bins
cargo build "${cargo_flags[@]}" -p silo-web --bin silo-web-helper
cargo build "${cargo_flags[@]}" -p silo-app "${app_flags[@]}"

out="$root/target/$profile"
case "$("$out/silo" --version)" in
  *" dev") echo "bundle-macos: $out/silo is a dev-channel build" >&2; exit 1 ;;
esac
app="$root/target/Silo.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$out/silo-app" "$app/Contents/MacOS/Silo"
# The app icon (scripts/make-icons.sh): Assets.car for macOS 26 and later,
# Silo.icns before; shared with Silo Server.app below.
cp "$root/packaging/macos/Assets.car" "$root/packaging/macos/Silo.icns" "$app/Contents/Resources/"
# The server/CLI: Contents/Helpers/Silo Server.app/Contents/MacOS/silo. It is
# the responsible process of every shell (it disclaims whoever started it,
# crates/silo-cli/src/disclaim.rs), so privacy prompts and System Settings
# name it, with usage strings worded for terminal programs
# (scripts/server-info-plist.sh).
server_app="$app/Contents/Helpers/Silo Server.app"
mkdir -p "$server_app/Contents/MacOS" "$server_app/Contents/Resources"
cp "$out/silo" "$server_app/Contents/MacOS/silo"
# Menu-bar agent (accessory app, no Dock icon):
# Contents/Helpers/Silo Menu Bar.app/Contents/MacOS/silo-menubar. In the
# app's Contents/MacOS, AppKit would take it for Silo.app itself (same bundle
# id) and Launch Services could send the app's reopen and open events to it
# (scripts/menubar-info-plist.sh). Under launchd the app runs copies of both
# helper bundles out of <SILO_HOME>/runtime (one fixed place), so the app
# can be replaced while they run.
menubar_app="$app/Contents/Helpers/Silo Menu Bar.app"
mkdir -p "$menubar_app/Contents/MacOS" "$menubar_app/Contents/Resources"
cp "$out/silo-menubar" "$menubar_app/Contents/MacOS/silo-menubar"
# The otto daemon agent blocks connect to (started by the app).
cp "$out/otto-silo" "$app/Contents/MacOS/otto-silo"
# Browser engine: CEF framework + four helper apps (copies of
# silo-web-helper), as silo-web's README describes.
cef="${CEF_PATH:-$("$root/scripts/fetch-cef.sh")}"
framework="Chromium Embedded Framework.framework"
mkdir -p "$app/Contents/Frameworks"
ditto "$cef/$framework" "$app/Contents/Frameworks/$framework"
helpers=()
for suffix in "" " (GPU)" " (Renderer)" " (Plugin)"; do
  name="Silo Helper$suffix"
  dir="$app/Contents/Frameworks/$name.app/Contents"
  mkdir -p "$dir/MacOS"
  cp "$out/silo-web-helper" "$dir/MacOS/$name"
  id="xyz.nitish.silo.helper$(echo "$suffix" | tr -d ' ()' | tr 'A-Z' 'a-z')"
  cat > "$dir/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$id</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSUIElement</key><true/>
</dict></plist>
PLIST
  helpers+=("$app/Contents/Frameworks/$name.app")
done
# The app finds the server at ../Helpers/Silo Server.app/Contents/MacOS/silo
# (crates/silo-app/src/conn.rs); in Contents/MacOS `silo` and `Silo` would
# collide on a case-insensitive volume. Both helpers get the app's icon.
icon=""
if [[ -f "$app/Contents/Resources/Assets.car" ]]; then
  for helper_app in "$server_app" "$menubar_app"; do
    cp "$app/Contents/Resources/Assets.car" "$app/Contents/Resources/Silo.icns" "$helper_app/Contents/Resources/"
  done
  icon=Silo
fi
"$root/scripts/server-info-plist.sh" "Silo Server" xyz.nitish.silo.server "$version" ${icon:+"$icon"} > "$server_app/Contents/Info.plist"
"$root/scripts/menubar-info-plist.sh" "Silo Menu Bar" xyz.nitish.silo.menubar "$version" ${icon:+"$icon"} > "$menubar_app/Contents/Info.plist"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Silo</string>
  <key>CFBundleDisplayName</key><string>Silo</string>
  <key>CFBundleIdentifier</key><string>xyz.nitish.silo</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleExecutable</key><string>Silo</string>
  <key>CFBundleIconFile</key><string>Silo</string>
  <key>CFBundleIconName</key><string>Silo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Web pages in Silo can use your phone or a security key to sign in.</string>
  <key>NSCameraUsageDescription</key><string>Web pages in Silo can ask to use the camera.</string>
  <key>NSMicrophoneUsageDescription</key><string>Silo listens while you dictate with the Fn key, on this Mac, and web pages in Silo can ask to use the microphone.</string>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>xyz.nitish.silo</string>
      <key>CFBundleURLSchemes</key><array><string>silo</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# Inside out, with Chrome's entitlements: the Renderer and GPU helpers JIT,
# and the app (the responsible process for its helpers) may use the camera
# and microphone. Everything carries one team ID, so library validation
# holds. Ad-hoc signatures have no team ID, so they skip the hardened
# runtime.
identity="${SILO_SIGN_IDENTITY:--}"
if [[ "$identity" == - ]]; then
  sign() { codesign --force --sign - --timestamp=none "$@"; }
else
  sign() { codesign --force --sign "$identity" --options runtime --timestamp "$@"; }
fi
entitlements() { # file keys...
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    for key in "${@:2}"; do echo "  <key>$key</key><true/>"; done
    echo '</dict></plist>'
  } > "$1"
}
jit="$root/target/helper-jit.entitlements"
entitlements "$jit" com.apple.security.cs.allow-jit
devices="$root/target/app.entitlements"
entitlements "$devices" com.apple.security.device.audio-input com.apple.security.device.camera
# What programs in Silo's terminals may ask for under the hardened runtime
# (the server is their responsible process).
terminal="$root/target/server.entitlements"
entitlements "$terminal" com.apple.security.automation.apple-events \
  com.apple.security.device.audio-input com.apple.security.device.camera \
  com.apple.security.personal-information.addressbook com.apple.security.personal-information.calendars \
  com.apple.security.personal-information.location com.apple.security.personal-information.photos-library
for lib in "$app/Contents/Frameworks/$framework/Libraries/"*.dylib; do
  sign "$lib"
done
sign "$app/Contents/Frameworks/$framework"
for helper in "${helpers[@]}"; do
  case "$helper" in
    *"(Renderer).app" | *"(GPU).app") sign --entitlements "$jit" "$helper" ;;
    *) sign "$helper" ;;
  esac
done
sign --entitlements "$terminal" "$server_app"
codesign --verify --strict "$server_app"
sign "$menubar_app"
codesign --verify --strict "$menubar_app"
sign "$app/Contents/MacOS/otto-silo"
sign --entitlements "$devices" "$app"
codesign --verify --deep --strict "$app"
echo "built $app"
