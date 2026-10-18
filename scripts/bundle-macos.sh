#!/usr/bin/env bash
# Build target/Silo.app: the GPUI client, the `silo` server/CLI binary (as
# silo-server) and the silo-menubar agent in Contents/MacOS, ad-hoc signed.
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
cargo build "${cargo_flags[@]}" -p silo-app "${app_flags[@]}"

out="$root/target/$profile"
app="$root/target/Silo.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$out/silo-app" "$app/Contents/MacOS/Silo"
cp "$out/silo" "$app/Contents/MacOS/silo-server"
# Menu-bar agent (accessory app, no Dock icon), started by the app and by
# `silo-server server --daemon`.
cp "$out/silo-menubar" "$app/Contents/MacOS/silo-menubar"
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

codesign --force --sign - --timestamp=none "$app/Contents/MacOS/silo-server"
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/silo-menubar"
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/Silo"
codesign --force --sign - --timestamp=none "$app"
codesign --verify --deep --strict "$app"
echo "built $app"
