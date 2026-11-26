---
name: assembly
description: Cutting an episode or piece with ffmpeg through shell: reading shot order and the picked takes of the shots' cards from the canvas, normalising clips to the canvas format, joining with cuts or crossfades, mixing dialogue, SFX, ambience and music with ducking and loudness normalisation, subtitles (sidecar or burned), writing canvases/<name>/out/<name>-v<N>.mp4 without overwriting earlier cuts, a vertical cut from landscape, verifying with ffprobe and placing the cut on the canvas. Load when the user asks for a cut, an edit, an export, a rough cut, a trailer or a version for another format.
---

# Assembly

The cut is built in four passes: normalise each take, join the picture, mix the sound, finish (subtitles,
loudness, checks). Each pass writes files you can inspect, in a scratch folder, so a failure is cheap to find.

## 1. Preflight

```sh
command -v ffmpeg && command -v ffprobe && ffmpeg -hide_banner -version | head -1
```
If missing, tell the user and stop: macOS `brew install ffmpeg`, Debian/Ubuntu `sudo apt install ffmpeg`,
Windows `winget install ffmpeg`. Burned-in subtitles need the `subtitles` filter (libass): check
`ffmpeg -hide_banner -filters | grep -w subtitles`; Homebrew's plain `ffmpeg` lacks it (`brew install ffmpeg-full`
has it). Without it, deliver sidecar subtitles (§6) and say so.

Then gather:
1. `studio_read`: the canvas's effective format (canvas override, else project): aspect, resolution (the short
   side), fps (default 24 when unset). 16:9 1080p → 1920×1080; 9:16 1080p → 1080×1920; 1:1 → 1080×1080;
   4:5 → 1080×1350; 720p halves the short side's 1080 to 720.
2. `canvas_read {canvas}`: scenes by `order`, shots by `order` inside each. Per shot frame:
   - **The clip**: the picked take of the shot's video card. With several video cards in the frame (a lip-sync
     or extend card built on `S2.3 video`), the last of the chain: the one no other card there takes as
     `video`. Trims from the note's `Cut:` line when its take file is the picked take; otherwise check the
     clip and trim only the handle.
   - **Several `Cut:` lines** in a note: the shot is those passages in order, each from the take file its line
     names (any take of a video card in the frame, picked or not). Normalise each passage to its own
     `$T/NN.mp4` (§2), numbered so they sort in order. This is how a performance picked from several takes, or
     a pass cut together with its re-coverage and its finish, reaches the cut.
   - **A pass frame** (note title `P3A …`, plan `pass`) is one clip like any shot; its cuts are inside the
     clip. An extend card's take may hold the whole clip or only the continuation: `ffprobe` its duration, and
     when it is only the continuation use both clips, as two `Cut:` passages.
   - **An impact insert** (a `still` plan with a length in frames in its note): the key card's take held for
     that length, no push (§2).
   - **Audio**: the note's `Audio:` entries, each a card title → that card's picked take, offset, role.
   - No video take (draft, failed, `still` plan): the key card's picked take as a still (§2); list it in the
     report. Mention cards that show Out of date: the user may want them re-run first.
   Then the `**Mix · <canvas>**` note (`sound-design`), its music and ambience by card title.
3. Next version number: `ls canvases/<ep>/out/` and take the highest `-v<N>` + 1. Never overwrite a cut.

Work in a scratch folder: `T=$(mktemp -d)`, and run every command from the studio root.

## 2. Normalise every take

Generated clips differ in size, fps, pixel format, timebase and whether they have audio at all. The concat
step needs them identical. Per shot, in order, write `$T/NN.mp4`:

```sh
W=1920; H=1080; FPS=24          # from the format
norm() {  # norm <in> <out> <in-seconds> <out-seconds>
  local in="$1" out="$2" ss="${3:-0}" to="$4" amap=0:a:0; local trim=(-ss "$ss") silent=()
  [ -n "$to" ] && trim+=(-to "$to")
  if [ -z "$(ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 "$in")" ]; then
    silent=(-f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000); amap=1:a:0
  fi
  ffmpeg -hide_banner -loglevel error -y "${trim[@]}" -i "$in" "${silent[@]}" -map 0:v:0 -map "$amap" \
    -vf "scale=${W}:${H}:force_original_aspect_ratio=increase,crop=${W}:${H},setsar=1,fps=${FPS},format=yuv420p" \
    -af "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,apad" -shortest \
    -c:v libx264 -preset medium -crf 18 -c:a aac -b:a 192k -ar 48000 \
    -video_track_timescale $((FPS*1000)) -movflags +faststart "$out"
}
norm canvases/ep01-pilot/assets/<S2.1 picked take>.mp4 "$T/01.mp4" 0.4 3.4    # Cut: in 0.4 out 3.4
```
- `scale…increase,crop`: fill the frame, crop the overflow (centred). Use `force_original_aspect_ratio=decrease,
  pad=${W}:${H}:(ow-iw)/2:(oh-ih)/2` instead to letterbox rather than crop.
- `fps=`: constant frame rate; fixes variable-frame-rate sources. `setsar=1` and `format=yuv420p`: square pixels
  and the pixel format every player takes. `-video_track_timescale`: the same timebase on every file, so
  concat doesn't stutter or drift.
- Silent clips get a silent stereo track (`anullsrc`), because concat and the mix need audio in every file.
  `apad` + `-shortest` make each file's audio exactly as long as its video.
- Shots whose clip sound the Mix note mutes: add `-af "volume=0,…"` or feed `anullsrc` instead.

**A still as a shot** (a keyframe with a slow push in), `D` seconds:
```sh
ffmpeg -hide_banner -loglevel error -y -loop 1 -framerate $FPS -t $D -i <keyframe.png> \
  -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 -map 0:v -map 1:a -shortest \
  -vf "scale=$((W*2)):$((H*2)):force_original_aspect_ratio=increase,crop=$((W*2)):$((H*2)),zoompan=z='min(1+0.0015*on,1.08)':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=1:s=${W}x${H}:fps=${FPS},setsar=1,format=yuv420p" \
  -c:v libx264 -preset medium -crf 18 -c:a aac -b:a 192k -ar 48000 -video_track_timescale $((FPS*1000)) "$T/NN.mp4"
```
(Upscaling 2× before `zoompan` avoids its jitter.) An impact insert is the same still held without the push,
`N` frames long: `-t $(awk -v n=$N -v f=$FPS 'BEGIN{print n/f}')` and `scale…,crop=${W}:${H},setsar=1,
fps=${FPS},format=yuv420p` in place of the `zoompan` chain. Hard cuts on both sides, never a fade.

**Cuts on the music's beat** (action cut to a track, `animation-direction` §7): the choreography document gives
the beat times. Choose each passage's `in`/`out` so its length is a whole number of beats (at 120 BPM 0.5 s, at
90 BPM 0.667 s) and the first cut lands on a beat of the cue's offset in the Mix note.

Then the timeline: each shot's start in the cut, used for audio offsets and subtitles:
```sh
t=0; for f in "$T"/[0-9][0-9].mp4; do d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f")
  printf '%s %.3f %.3f\n' "$(basename "$f")" "$t" "$d"; t=$(awk -v a="$t" -v b="$d" 'BEGIN{print a+b}'); done | tee "$T/timeline.txt"
```

## 3. Join the picture

**Straight cuts** (the default): concat demuxer, no re-encode. The list's paths are relative to the list file.
```sh
(cd "$T" && ls [0-9][0-9].mp4 | sed "s/.*/file '&'/" > list.txt)
ffmpeg -hide_banner -loglevel error -y -f concat -safe 0 -i "$T/list.txt" -c copy "$T/picture.mp4"
```

**Crossfades** (scene changes, time passing; not inside a dialogue scene): `xfade` for video, `acrossfade` for
the clips' own audio. Each fade overlaps two clips, so the cut gets shorter by the fade length. Offset of fade k
= (sum of the first k clip durations) − k × fade. For 3 clips of 4.583, 4.0, 6.0 s and 0.5 s fades: 4.083, then
7.583.
```sh
ffmpeg -hide_banner -loglevel error -y -i "$T/01.mp4" -i "$T/02.mp4" -i "$T/03.mp4" -filter_complex \
 "[0:v][1:v]xfade=transition=fade:duration=0.5:offset=4.083[v1];[v1][2:v]xfade=transition=fade:duration=0.5:offset=7.583,format=yuv420p[v];
  [0:a][1:a]acrossfade=d=0.5[a1];[a1][2:a]acrossfade=d=0.5[a]" \
 -map "[v]" -map "[a]" -c:v libx264 -preset medium -crf 18 -c:a aac -b:a 192k "$T/picture.mp4"
```
Other transitions: `fadeblack` (dip to black), `dissolve`, `wipeleft`. Mixing cuts and fades: concat the runs of
straight cuts first, then xfade the runs. With fades, recompute the timeline for audio offsets.

## 4. Mix the sound

Inputs: the picture (its audio = the clips' own sound), then the picked takes of the cards in the `Audio:` lines
(dialogue, SFX) and the Mix note (ambience, music). Each line's delay = its shot's start (timeline) + its
`@offset`, in milliseconds. Template for 2 lines, 1 SFX, an ambience
bed and a music cue, cut length `L` seconds:

```sh
ffmpeg -hide_banner -loglevel error -y -i "$T/picture.mp4" \
  -i <S2.3 mara-01 take> -i <S2.5 teo-01 take> -i <S2.4 sfx-bell take> \
  -stream_loop -1 -i <Sc2 amb-harbor-fog take> -i <M1 harbor-dawn take> -filter_complex "
  [1:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,adelay=8100:all=1[d1];
  [2:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,adelay=14100:all=1[d2];
  [d1][d2]amix=inputs=2:duration=longest:normalize=0,apad[dlg];
  [dlg]asplit=2[dlg_out][dlg_key];
  [3:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,adelay=11000:all=1,volume=-8dB[sfx];
  [4:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,atrim=0:L,volume=-20dB,afade=t=in:d=1[amb];
  [5:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,volume=-16dB,afade=t=in:d=1[mus];
  [mus][dlg_key]sidechaincompress=threshold=0.02:ratio=8:attack=20:release=350[mus_duck];
  [0:a]volume=-3dB[nat];
  [nat][dlg_out][sfx][amb][mus_duck]amix=inputs=5:duration=first:normalize=0,afade=t=out:st=L-2:d=2,
   loudnorm=I=-14:TP=-1.5:LRA=11,aresample=48000[a]" \
  -map 0:v -map "[a]" -c:v copy -c:a aac -b:a 192k -movflags +faststart "$T/mixed.mp4"
```
Replace `L` and `L-2` with numbers. What each part does:
- `aresample,aformat`: every source to 48 kHz stereo float so the mixers accept them.
- `adelay=…:all=1`: places a sound at its time on all channels. `apad`: dialogue runs to the end, so the
  sidechain key never ends early.
- `asplit` + `sidechaincompress`: music ducks while dialogue plays (threshold ≈ −34 dBFS, 8:1, 20 ms attack,
  350 ms release: about 8–10 dB of ducking). Duck ambience too by keying it the same way if it masks lines.
- `amix … normalize=0`: sums without dividing by the input count (which would make everything quiet);
  `duration=first` ends with the picture. `-stream_loop -1` loops a short ambience bed; `atrim` stops it.
- `loudnorm=I=-14:TP=-1.5:LRA=11`: −14 LUFS integrated, −1.5 dBTP for web and social; use −16 for talky
  pieces, −23 for broadcast. Levels come from the Mix note; the volumes above are its defaults.
- For an exact target on the final file, two-pass: run once with `-af loudnorm=I=-14:TP=-1.5:LRA=11:print_format=json
  -f null -`, then pass its `input_i`, `input_tp`, `input_lra`, `input_thresh`, `target_offset` as
  `measured_I`, `measured_TP`, `measured_LRA`, `measured_thresh`, `offset`, plus `linear=true`, in the second.

## 5. Write the cut

`cp "$T/mixed.mp4" canvases/<ep>/out/<ep>-v<N>.mp4` (or the subtitled file, §6), e.g.
`canvases/ep01-pilot/out/ep01-pilot-v3.mp4`. Then:

```sh
f=canvases/ep01-pilot/out/ep01-pilot-v3.mp4
ffprobe -v error -show_entries format=duration:stream=codec_type,codec_name,width,height,r_frame_rate,sample_rate,channels -of compact=p=0 "$f"
ffmpeg -hide_banner -nostats -i "$f" -af ebur128=peak=true -f null - 2>&1 | grep -A1 -E "Integrated|True peak" | tail -4
```
Check: one video stream at the format's size and fps, one AAC 48 kHz stereo stream, duration = timeline total
(± a frame; minus fades), loudness within 1 LU of the target. Grab a frame strip (`continuity` §5) and `read`
it to see the shots in order. Then place it on the canvas outside the scene frames:
`canvas_edit {canvas, ops: [{"op": "add_node", "node": {"body": {"kind": "asset", "path": "<f>", "media": "video",
"seconds": <duration>}}}]}`. Remove `$T` when done (`rm -rf "$T"`).

## 6. Subtitles

Build `canvases/<ep>/out/<ep>-v<N>.srt` from the script's lines and the timeline (start = shot start + line
offset; end = start + line duration from `ffprobe`; ≤ 42 characters per line, 2 lines max, ≥ 1 s on screen):
```
1
00:00:08,100 --> 00:00:09,400
Not again.
```
- **Sidecar / soft** (switchable): `ffmpeg -i "$T/mixed.mp4" -i <ep>.srt -map 0:v -map 0:a -map 1:s -c:v copy
  -c:a copy -c:s mov_text -metadata:s:s:0 language=eng <out>.mp4`; also keep the `.srt` beside the cut.
- **Burned** (social, vertical): `-vf "subtitles=<ep>.srt:force_style='FontName=Helvetica,FontSize=13,
  Outline=1,MarginV=60'" -c:a copy` (re-encodes video; needs libass). On 9:16 raise `MarginV` so captions
  sit above the bottom 20% of UI.

## 7. A second format from the same cut

Vertical 1080×1920 from a landscape cut, centred crop (subject must be inside the centre 9:16, as
`shot-listing` plans for):
```sh
ffmpeg -hide_banner -loglevel error -y -i <landscape cut> \
  -vf "crop=w=ih*9/16:h=ih:x='max(0,min(iw-ow,iw*0.5-ow/2))':y=0,scale=1080:1920,setsar=1,fps=30,format=yuv420p" \
  -c:v libx264 -preset medium -crf 18 -c:a copy -movflags +faststart <vertical cut>
```
Change `0.5` per shot to follow the subject (0.33 = left third): normalise each shot with its own `x` and
concat, rather than one crop for the whole cut. Where a wide shot can't be cropped, a blurred-fill frame:
`-filter_complex "[0:v]split=2[bg][fg];[bg]scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,
gblur=sigma=40[b];[fg]scale=1080:-2[f];[b][f]overlay=(W-w)/2:(H-h)/2,setsar=1,fps=30,format=yuv420p[v]"
-map "[v]" -map 0:a`. Write it to the vertical canvas's `out/` (or `<ep>-vertical-v<N>.mp4`).

## Pitfalls

- Concat of unnormalised clips: frozen frames, desync, or a failure halfway. Always normalise first.
- A clip with no audio stream breaks `concat` and `acrossfade`: add `anullsrc` (§2).
- `-ss` after `-i` decodes from the start (slow) but is frame-exact; before `-i` it is fast and, with
  re-encoding, accurate enough. Trims here re-encode, so before `-i` is fine.
- `amix` without `normalize=0` lowers every input by the input count.
- Variable frame rate sources: `fps=` in the normalise pass; never `-c copy` them into the concat.
- Overwriting `out/` files: always a new `-v<N>`.

## Next

Report the file, its length, what is still placeholder (stills, missing lines), and offer the other format or
a v<N+1> after notes. Fixes to individual shots → `video-prompting`, `continuity`; to the mix → `sound-design`.
