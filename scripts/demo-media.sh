#!/usr/bin/env bash
# Turn a screen recording into the README's demo: docs/media/demo.gif
# (smooth, small enough for GitHub to show inline) and docs/media/demo.mp4.
#
#   scripts/demo-media.sh recording.mov [start] [end]
#     start/end: seconds or hh:mm:ss to trim to (default: the whole clip)
#
# Environment: FPS (default 30), WIDTH in px (default 1200; record a
# Retina window so it downscales sharply), OUT (default docs/media).
# Uses gifski when installed (`brew install gifski`: the best GIF quality),
# else ffmpeg's two-pass palette. Aim for under ~10 MB: GitHub may not
# show a bigger GIF inline. Lower FPS or WIDTH, or trim, if it is larger.
set -euo pipefail
cd "$(dirname "$0")/.."

in=${1:?usage: scripts/demo-media.sh recording.mov [start] [end]}
start=${2:-}
end=${3:-}
fps=${FPS:-30}
width=${WIDTH:-1200}
out=${OUT:-docs/media}
mkdir -p "$out"

trim=()
[[ -n "$start" ]] && trim+=(-ss "$start")
[[ -n "$end" ]] && trim+=(-to "$end")
scale="fps=$fps,scale=$width:-2:flags=lanczos"

echo "== mp4"
ffmpeg -hide_banner -loglevel error -y "${trim[@]}" -i "$in" -an -vf "$scale" \
    -c:v libx264 -preset slow -crf 20 -pix_fmt yuv420p -movflags +faststart "$out/demo.mp4"

echo "== gif"
if command -v gifski >/dev/null; then
    gifski --fps "$fps" --width "$width" --quality 90 -o "$out/demo.gif" "$out/demo.mp4"
else
    palette=$(mktemp -t silo-palette).png
    ffmpeg -hide_banner -loglevel error -y -i "$out/demo.mp4" -vf "palettegen=max_colors=256:stats_mode=diff" "$palette"
    ffmpeg -hide_banner -loglevel error -y -i "$out/demo.mp4" -i "$palette" \
        -lavfi "paletteuse=dither=sierra2_4a:diff_mode=rectangle" -loop 0 "$out/demo.gif"
    rm -f "$palette"
fi

ls -lh "$out/demo.mp4" "$out/demo.gif"
