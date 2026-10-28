#!/usr/bin/env bash
# Print the Info.plist of the menu-bar agent's helper app (Silo.app/Contents/
# Helpers/<name>.app, crates/silo-client/src/runtime.rs):
#   scripts/menubar-info-plist.sh NAME BUNDLE_ID VERSION [ICON]
# ICON names both Resources/ICON.icns and the Assets.car icon (scripts/make-icons.sh).
# A bundle of its own: run from the app's Contents/MacOS, AppKit took the
# agent for the app itself (same bundle id), and Launch Services sent the
# app's reopen and open events to the windowless agent.
set -euo pipefail

name="$1" id="$2" version="$3" icon="${4:-}"
cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$id</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>silo-menubar</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
${icon:+  <key>CFBundleIconFile</key><string>$icon</string>
  <key>CFBundleIconName</key><string>$icon</string>
}</dict>
</plist>
PLIST
