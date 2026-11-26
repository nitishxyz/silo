---
name: continuity
description: Keeping characters, wardrobe, props, places, light, time of day, screen direction and eyelines consistent across shots, scenes and episodes: which entity sheets and refs to pass as references, chaining a shot's last frame into the next shot's first_frame with ffmpeg, the per-scene continuity note, checking every result by looking at it, and re-running Out of date shots after a world change. Load before making a scene's keyframes or videos, when chaining shots, when a sheet or entity changed, or when results drift.
---

# Continuity

Models have no memory between calls. Everything that must stay the same has to be passed again every time:
the same reference images, the same appearance words, the same light, the same direction of travel. This skill
is how to do that without thinking about it shot by shot.

## 1. The scene's continuity note

Before the first keyframe of a scene, fill the `**Continuity · Scene N**` note in its scene frame (made by
`shot-listing`; `canvas_edit` `update_node {id, body: {"text": …}}`). Read the scene's shot notes, the entity
files and the previous scene's note first.

```markdown
**Continuity · Scene 2** (follows Scene 1 directly, same morning)
- Time / light: dawn, ~06:10. Sun low from screen-left in the wide (east = left of the harbor master's view).
  Cool fog, warm rim on faces from the left. No hard shadows.
- Weather: dense fog, still air, wet surfaces.
- Mara (characters/mara): default outfit (oilskin coat, sleeves rolled), hair wet and flat, compass outside
  the jumper. Holding: rope (until S2.3), then nothing.
- Teo (characters/teo): cabin outfit, no jacket, mug in right hand.
- Geography: boat bow points screen-right. Bell tower across the water, screen-right of Mara.
- Axis: Mara left of frame looking right; Teo behind her, right of frame looking left. Keep the camera on the
  dock side of the line between them.
- Changes during the scene: rope dropped at S2.3 and stays on the deck.
```

**Changes are one-way.** Whatever is dropped, broken, torn, spilled or lost stays that way in every later shot
of the scene, and models forget it unless told. Write each change with the shot it happens in, and state it as
a fact in every later prompt (`the rope lies coiled on the deck by her feet`). After a pass
(`action-choreography`), its note's `Ends` line is the state the next shot starts from: copy it here. A part
of a conversation that must start in the same breath as the last continues from that take's final seconds
(`dialogue-scenes` §7).

Copy the relevant lines into every prompt of the scene (light and weather sentences word for word). When the
scene changes something that lasts (a torn sleeve, a lost compass), write it into the entity file as a dated
line (`From ep 1 sc 2: compass lost.`) in the same turn.

## 2. References for every shot

Pass references on every keyframe and every reference-to-video card that shows an entity: on a card, mention it
in the prompt (`@characters/mara` wires her Entity node's sheet; `@characters/mara#hood-down` one of her named
references, an asset node of its file; a reference's id is its file's name without the extension, `refs/hood-down.png`) and the run names each with its image number and, for a named reference,
its `defines`; in a direct call, `references`, numbered by hand in the prompt ("image 3 defines only…").

| Shot shows | Pass |
|---|---|
| A character, front or 3/4 | `sheet.png` |
| A profile, back or full-body turn | `sheet.png` + the matching turn (`#turn-<angle>`) |
| A close-up | `sheet.png` + `#face`, or an expression reference for a strong emotion |
| A non-default outfit or state | that named reference (`#outfit-drysuit`, `#hood-down`), and the sheet for the face |
| A place | its `sheet.png` (establishing) or the named view or plate for that angle and time of day (`#reverse`, `#night`) |
| A prop that matters | its `sheet.png` |
| The look | a style frame from `world/style/` only when the model drifts from `style.md` |

- Read them from `studio_read` (each entity's references by id, with what each `defines`, and its `links`) or the
  entity frontmatter (`sheet`, `refs`); never use images in `refs/` that the frontmatter doesn't list. The shot
  note's `Who:` and `Where:` name the reference when it isn't the sheet (`shot-listing`); a prop is the one the
  character's `links` name.
- Within model limits (Gemini Omni 6 images, at most 3 per character; Kling elements 1 frontal + 1–3 more;
  others in `media_model_schema`): characters first, then the place, then props.
- In the prompt, describe each character with the appearance words of their file and say which reference is
  whom when the model numbers references (`video-prompting` per model). Give each reference its one job and
  say what stays behind in it (a sheet's white background and flat light, a plate's weather), so only the
  intended part carries over.
- On image-to-video the keyframe carries identity; add the sheet as a reference only where the model takes
  references alongside a first frame, and always when the face drifted in an earlier try.

## 3. Screen direction, eyelines, light

- **180° rule**: in a scene, keep the camera on one side of the line between the two people talking (or along
  a subject's direction of travel). A shot from the other side flips who is left and right; only cross with a
  shot on the line (a POV, a dolly across) or a neutral cut (a head-on shot, an insert).
- **Direction of travel**: a character leaving screen-right enters the next shot from screen-left. Write the
  side in the prompt (`walks from left to right of frame`).
- **Eyelines**: in shot/reverse-shot, if A looks screen-right, B looks screen-left, both slightly off-lens at the
  same height (adjust for standing vs seated). A POV shot follows a look: the shot before shows the look, the POV
  shows what is seen from roughly that height.
- **Light**: keep the key on the same side relative to the geography, not the camera. When the reverse angle
  turns the camera around, the key moves to the other side of frame. Same colour temperature, same weather.
- **Time across scenes**: dawn → morning → noon light changes; write the time in each scene note.

## 4. Chaining: last frame → next first frame

Within a scene, when action continues across a cut at the same or a close angle (she walks out of S2.5 and
into S2.6), use the previous clip's last frame as the next shot's `first_frame`. For a new angle, don't chain:
make a fresh keyframe with the same references.

1. Grab the last frame of the picked take of `S2.5 video` (its path from `canvas_read`; this writes the final
   decoded frame) into `assets/frames/`:
   ```sh
   mkdir -p canvases/ep01-pilot/assets/frames
   ffmpeg -hide_banner -loglevel error -y -sseof -0.3 -i "canvases/ep01-pilot/assets/<S2.5 picked take>.mp4" \
     -update 1 canvases/ep01-pilot/assets/frames/s02-05-last.png
   ```
   For a frame at a time (a cleaner frame before a blur, or the `out` point of the shot's `Cut:` line):
   `ffmpeg -hide_banner -loglevel error -y -ss 4.2 -i <clip> -frames:v 1 <out.png>`.
2. `read` the frame. If it is soft, blurred or the face is off, use an earlier frame and set that time as the
   `out` of the previous shot's `Cut:` line, or repair the frame with an image edit that passes the sheet
   (`image-prompting`). A bad first frame poisons the whole next clip.
3. Put it on the canvas in the next shot's frame and wire it, so the lineage shows: `canvas_edit` `add_node
   {parent: "<S2.6 frame id>", body: {"kind": "asset", "path": "canvases/ep01-pilot/assets/frames/s02-05-last.png",
   "media": "image"}}`, then `add_edge` from it to the `S2.6 video` card as `first_frame` (it replaces a
   wire from a key card; a chained shot needs no keyframe). Keep the sheets wired as `reference` where the
   model takes both.
4. The video card's prompt starts from the continuing action: `She keeps walking toward screen-right and stops
   at the rail.` When S2.5 later gets a new picked take, grab its last frame again to the same path: the file
   changes, so `S2.6 video` shows Out of date.
5. Chain at most 3–4 shots before going back to a fresh keyframe from the sheets: each generation drifts a
   little and chaining accumulates it.

Generated clips may end on a slightly different frame than `last_frame` asked for; always grab from the clip,
not from the keyframe you passed.

## 5. Look before moving on

You see images; you don't see video. So:
- Keyframes: `read` each one. Compare with the sheet and the continuity note: face, hair, anchors (scar, coat,
  boots), outfit state, props in hand, number of people, light side, screen side, text in frame.
- Videos: grab a strip of six evenly spaced frames and `read` it:
  ```sh
  f="canvases/ep01-pilot/assets/<clip>.mp4"
  d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f")
  ffmpeg -hide_banner -loglevel error -y -i "$f" -vf "fps=6/$d,scale=480:-2,tile=6x1" -frames:v 1 /tmp/strip.png
  ```
  Look for faces changing over time, extra limbs, morphing hands, sudden cuts, a drifting outfit.
- Say in one line what is off and fix it (re-run with stronger references, a shorter clip, an edit) before
  generating anything that builds on it.

## 6. When the world changes

When a sheet or ref is remade (the user restyles Mara's coat), every card wired from it shows Out of date
("Mara's sheet changed"), and so do the cards wired from those once they get new takes. Nothing reruns on its
own.

1. Make sure the old sheet was archived (`character-design` keeps `refs/archive/sheet-v1.png`).
2. `canvas_read` each canvas (from `studio_read`) and list its Out of date cards, in shot order. Also read the
   shot notes whose `Who`/`Where`/`Props` name the entity: a card that shows it without a wire (an entity file's
   text changed, the style line changed, a text-only card) is not marked.
3. Tell the user which shots are affected and what a re-run costs (count of keyframes and clips); re-run when
   they agree or asked for it. Leave out cards whose pick the user chose, unless they say so.
4. `canvas_run` the key cards first; look at them (§5); then grab new last frames for chained shots (§4) and
   `canvas_run` the video cards. The cards keep their prompts and wires; each gets a new take, and the old
   takes stay on the card to compare or pick back.
5. A `Cut:` line in a shot note was for the old take: check the trim against the new one and update it.

## Across episodes

- Read the previous episode's last scene continuity note before writing the first of the next one.
- Wardrobe and injuries follow the dated lines in entity files; a new outfit gets its own ref before use.
- Recurring places keep their geography (which way the harbor faces); write it in the place file once.

## Next

Prompting the chained or referenced shot → `video-prompting`. Sound that crosses cuts → `sound-design`.
The cut → `assembly`.
