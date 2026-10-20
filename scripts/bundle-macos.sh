#!/usr/bin/env bash
# Build target/Silo.app: the GPUI client, the `silo` server/CLI binary (as
# silo-server) and the silo-menubar agent in Contents/MacOS.
#
# Signed ad-hoc, or with SILO_SIGN_IDENTITY (a Developer ID Application
# identity in the keychain) under the hardened runtime with secure
# timestamps, ready for notarization (.github/workflows/release.yml).
#
#   scripts/bundle-macos.sh            # release build
#   PROFILE=debug scripts/bundle-macos.sh
#   SILO_APP_FEATURES="" scripts/bundle-macos.sh   # precompiled Metal shaders (needs Xcode Metal toolchain)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

profile="${PROFILE:-release}"
features="${SILO_APP_FEATURES-runtime_shaders}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

cargo_flags=()
[[ "$profile" == release ]] && cargo_flags+=(--release)
app_flags=(--no-default-features)
[[ -n "$features" ]] && app_flags+=(--features "$features")

cargo build "${cargo_flags[@]}" -p silo-cli -p silo-menubar
cargo build "${cargo_flags[@]}" -p silo-web --bin silo-web-helper
cargo build "${cargo_flags[@]}" -p silo-app "${app_flags[@]}"

out="$root/target/$profile"
app="$root/target/Silo.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$out/silo-app" "$app/Contents/MacOS/Silo"
cp "$out/silo" "$app/Contents/MacOS/silo-server"
# Menu-bar agent (accessory app, no Dock icon). Under launchd the app runs
# copies of both out of <SILO_HOME>/runtime/<build> as `silo` and
# `silo-menubar`, so the bundle can be replaced while they run.
cp "$out/silo-menubar" "$app/Contents/MacOS/silo-menubar"
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
# On a case-insensitive volume `silo` and `Silo` collide, so the server/CLI
# binary ships as `silo-server`; the app looks for it next to itself.
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
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Web pages in Silo can use your phone or a security key to sign in.</string>
  <key>NSCameraUsageDescription</key><string>Web pages in Silo can ask to use the camera.</string>
  <key>NSMicrophoneUsageDescription</key><string>Web pages in Silo can ask to use the microphone.</string>
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
sign "$app/Contents/MacOS/silo-server"
sign "$app/Contents/MacOS/silo-menubar"
sign --entitlements "$devices" "$app"
codesign --verify --deep --strict "$app"
echo "built $app"
