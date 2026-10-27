#!/usr/bin/env bash
# Print the Info.plist of the server's helper app (Silo.app/Contents/Helpers/
# <name>.app, crates/silo-client/src/runtime.rs):
#   scripts/server-info-plist.sh NAME BUNDLE_ID VERSION [ICON]
# ICON names both Resources/ICON.icns and the Assets.car icon (scripts/make-icons.sh).
# The server is the responsible process of every program run in Silo's
# terminals (crates/silo-cli/src/disclaim.rs), so macOS privacy prompts show
# these usage strings, worded for those programs.
set -euo pipefail

name="$1" id="$2" version="$3" icon="${4:-}"
terminal="A program running in a Silo terminal"
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
  <key>CFBundleExecutable</key><string>silo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSUIElement</key><true/>
${icon:+  <key>CFBundleIconFile</key><string>$icon</string>
  <key>CFBundleIconName</key><string>$icon</string>
}  <key>NSAppleEventsUsageDescription</key><string>$terminal wants to control another app. Silo does not control other apps itself.</string>
  <key>NSAudioCaptureUsageDescription</key><string>$terminal wants to record audio from other apps. Silo does not record audio itself.</string>
  <key>NSBluetoothAlwaysUsageDescription</key><string>$terminal wants to use Bluetooth. Silo does not use Bluetooth itself.</string>
  <key>NSCalendarsUsageDescription</key><string>$terminal wants to use your calendars. Silo does not use your calendars itself.</string>
  <key>NSRemindersUsageDescription</key><string>$terminal wants to use your reminders. Silo does not use your reminders itself.</string>
  <key>NSCameraUsageDescription</key><string>$terminal wants to use your camera. Silo does not use the camera itself.</string>
  <key>NSContactsUsageDescription</key><string>$terminal wants to use your contacts. Silo does not use your contacts itself.</string>
  <key>NSLocalNetworkUsageDescription</key><string>Silo uses the local network to reach your paired devices, and a program running in a Silo terminal can ask to use it too.</string>
  <key>NSLocationUsageDescription</key><string>$terminal wants to use your location. Silo does not use your location itself.</string>
  <key>NSMicrophoneUsageDescription</key><string>$terminal wants to use your microphone. Silo does not use the microphone itself.</string>
  <key>NSPhotoLibraryUsageDescription</key><string>$terminal wants to use your photos. Silo does not use your photos itself.</string>
</dict>
</plist>
PLIST
