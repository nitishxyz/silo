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
cargo build "${cargo_flags[@]}" -p silo-web --bin silo-web-helper
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

# Inside out. Renderer and GPU helpers JIT. Ad-hoc signatures have no team
# ID, so hardened runtime (library validation) waits for a real identity.
jit="$root/target/helper-jit.entitlements"
cat > "$jit" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.cs.allow-jit</key><true/>
  <key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
</dict></plist>
PLIST
sign() { codesign --force --sign - --timestamp=none "$@"; }
find "$app/Contents/Frameworks/$framework/Libraries" -name '*.dylib' -print0 | xargs -0 -I{} codesign --force --sign - --timestamp=none {}
sign "$app/Contents/Frameworks/$framework"
for helper in "${helpers[@]}"; do
  case "$helper" in
    *"(Renderer).app" | *"(GPU).app") sign --entitlements "$jit" "$helper" ;;
    *) sign "$helper" ;;
  esac
done
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/silo-server"
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/silo-menubar"
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/Silo"
codesign --force --sign - --timestamp=none "$app"
codesign --verify --deep --strict "$app"
echo "built $app"
