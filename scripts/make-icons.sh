#!/usr/bin/env bash
# Regenerate Silo's icons from packaging/macos/icon/generate.py:
#   packaging/macos/icon/svg/*.svg     the mark, its variants and flat app icons
#   packaging/macos/Silo.icon          the app icon in Icon Composer's format
#                                      (dark plate fill, mark and light-pool layers);
#                                      open it in Icon Composer to adjust
#   packaging/macos/Assets.car         Silo.icon compiled by actool, and
#   packaging/macos/Silo.icns          its fallback for macOS before 26, both
#                                      shipped in Silo.app and Silo Server.app
#                                      (CFBundleIconName / CFBundleIconFile Silo);
#                                      macOS 26 frames a plain .icns in a grey tile
#   crates/silo-menubar/assets/menubar{,@2x}.png   the menu-bar template
#
#   scripts/make-icons.sh
#
# Layers are rendered with AppKit (render-svg.swift), which keeps
# transparency. Needs Xcode 26 or later (actool with .icon support).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
mac="$root/packaging/macos"
src="$mac/icon"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

python3 "$src/generate.py" "$src/svg"
swiftc -O "$src/render-svg.swift" -o "$tmp/render-svg"
render() { "$tmp/render-svg" "$@"; }

# Silo.icon: on Icon Composer's 1024 pt canvas, layers sit centred at their
# pixel size. The mark is 470 pt tall (543 px of its 60-unit box), a little
# above centre, with the pool of light 278 pt below centre, under its base.
icon="$mac/Silo.icon"
rm -rf "$icon"
mkdir -p "$icon/Assets"
render "$src/svg/icon-layer-mark.svg" "$icon/Assets/mark.png" 543
render "$src/svg/icon-layer-pool.svg" "$icon/Assets/pool.png" 600 180
cat > "$icon/icon.json" <<'JSON'
{
  "fill" : {
    "linear-gradient" : [
      "srgb:0.13333,0.16471,0.21961,1.00000",
      "srgb:0.03922,0.05098,0.07451,1.00000"
    ]
  },
  "groups" : [
    {
      "layers" : [
        {
          "glass" : false,
          "image-name" : "mark.png",
          "name" : "Mark",
          "position" : { "scale" : 1, "translation-in-points" : [ 0, -12 ] }
        }
      ],
      "lighting" : "individual",
      "name" : "Mark",
      "shadow" : { "kind" : "neutral", "opacity" : 0.5 },
      "translucency" : { "enabled" : false, "value" : 0.5 }
    },
    {
      "layers" : [
        {
          "glass" : false,
          "image-name" : "pool.png",
          "name" : "Pool",
          "position" : { "scale" : 1, "translation-in-points" : [ 0, 278 ] }
        }
      ],
      "lighting" : "individual",
      "name" : "Pool",
      "shadow" : { "kind" : "neutral", "opacity" : 0 },
      "specular" : false,
      "translucency" : { "enabled" : false, "value" : 0.5 }
    }
  ],
  "supported-platforms" : { "squares" : "shared" }
}
JSON

mkdir -p "$tmp/car"
xcrun actool "$icon" --compile "$tmp/car" --platform macosx --minimum-deployment-target 12.0 \
  --app-icon Silo --output-partial-info-plist "$tmp/partial.plist" --output-format human-readable-text >/dev/null
cp "$tmp/car/Assets.car" "$mac/Assets.car"
cp "$tmp/car/Silo.icns" "$mac/Silo.icns"

menubar="$root/crates/silo-menubar/assets"
mkdir -p "$menubar"
render "$src/svg/silo-menubar-template.svg" "$menubar/menubar.png" 18
render "$src/svg/silo-menubar-template.svg" "$menubar/menubar@2x.png" 36
echo "icons: packaging/macos/{Silo.icon,Assets.car,Silo.icns}, crates/silo-menubar/assets/menubar{,@2x}.png"
