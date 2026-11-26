---
name: location-design
description: Creating or changing a place in a Studio: the place file in world/places/<id>/, its establishing image (the sheet), empty background plates, reverse angles, time-of-day and weather variants, interior/exterior, and a layout map when geography must stay stable across shots. Load for a new location, "Generate sheet" on a place, a scene set somewhere not yet designed, or when shots in one place disagree about what is where.
---

# Location design

A place stays consistent when its file fixes the geography in words, its sheet fixes the look, and every shot
there carries the sheet or the plate that fits. Read `world/style.md` and the scenes set there before designing.
Load `image-prompting` for prompt shape and multi-reference wording; this skill covers what is specific to places.

Reuse beats invention: every extra set multiplies the cross-shot consistency work. Before making a place, check
whether the scene can happen in one that exists. Aim for 3–4 places per episode.

## 1. The file

`world/places/<id>/<id>.md`:

```markdown
---
name: The Old Bell Quay
sheet: sheet.png
refs:
  - file: refs/reverse.png
    name: Reverse
    defines: the quay seen from the tower, looking back at the steps and the town
  - file: refs/night.png
    name: Night
    defines: the night light only; not its framing
  - refs/storm.png
  - file: refs/interior-bellhouse.png
    name: Bellhouse interior
tags: [exterior, harbor, recurring]
summary: Half-sunken granite quay in the flooded old town; a rusted iron bell tower leans over still green water; rope bridges between rooftops.
---
## Look
Grey granite quay, slick and weeded below the tide line, broken off at the far end. A three-storey iron bell
tower, rusted orange, leans 10° toward the water. Flooded townhouses on both sides with water up to the
second-floor windows; laundry lines and rope bridges between the rooftops. Still green water with floating
planks. Scale: the tower door is twice a person's height; the quay is four paces wide.

## Layout
From the quay steps (the main camera position), looking out to sea: the tower is ahead and left; open sea
behind it; the Guild office and the town to the right; the steps down to the water at the bottom of frame.
Reverse (from the tower looking back): the steps, then the town climbing the hill, the Guild office on the left.

## Light, times and weather
Default: overcast morning, soft light from the sea side (behind the tower). Night: sodium lamps on the Guild
side only. Storm: grey, spray over the quay. From ep 5: the tower has collapsed into the water.

## Sound
Lapping water, gulls, the tower's chain creaking. (Read by `sound-design`.)

## Scenes here
ep01 sc1, ep01 sc3, ep02 sc2.
```

- **Look** is prompt text: materials, colours, wear, and the 2–3 landmarks that make it this place from any
  angle (the leaning tower, the rope bridges). Give scale with the place's own parts (a door, a step, a table),
  not with people.
- **Layout** fixes directions from a named camera position, plus the reverse. Write it as soon as two shots
  in the place face different ways. Give the sides short names that prompts can reuse word for word ("the
  tower side", "the Guild side", "the water edge"): multi-shot action names every camera position and every
  body's place by them (`action-choreography` §4).
- **Light** names its sources and their direction (the sea side, the sodium lamps), not where light lands.
- Interior and exterior of one building: one entity when they appear together (`refs/interior-*.png`), two
  entities (`bellhouse-exterior`, `bellhouse-interior`) when they are separate sets.
- Keep one spelling of the place's name everywhere: file, scripts' scene headings, prompts.

## 2. The establishing image (the sheet)

A clean, empty wide view in the default light, at the project format's aspect. It is a file made with a direct
call, not a card: read `format` from `studio_read`, set `aspect_ratio` and end the prompt with the style line
yourself.

```
Wide establishing shot of <name>, deserted and still: <look paragraph>.
Foreground: <materials and dressing nearest the camera>. Middle ground: <the main landmark>. Background:
<what closes the view>.
Seen from <the main camera position from Layout>, eye level, <lens from style.md>.
<Light: the source and its direction, e.g. "soft overcast daylight from the sea side, behind the tower">.
<Style line from style.md>
```

- **Empty**: plates and sheets are referenced by every shot there, so any figure in them turns up as an extra.
  Say "deserted, empty quay" rather than listing what is absent.
- **Three planes of depth** give the video model room to place a camera and move.
- **No baked atmosphere** in the default sheet: fog, light beams, rain or a heavy grade appear in every shot
  that references it. Those go in variants used only by the scenes that need them.
- **Large shapes that survive video**: a few big readable masses (an arch, a slope of boulders, a dark opening
  as the focal point) with texture on them, rather than fine clutter, which swims when the camera moves.
- **A sheet lit to be read can serve a dark scene.** The shot's prompt then gives it the job of shape only:
  "Image 5 is the cave: its geometry and materials only. Ignore its light and exposure completely: here it is
  pitch-black except where the lamps reach." (Or keep a darkened copy as a variant; the user may grade it by
  hand, as with any sheet: `character-design` §3.)

Calls:
1. `generate_image {prompt, aspect_ratio, n: 3}`. When the place should match the style frame, add
   `references: ["world/style/frame-01.png"]` with "Match the palette and rendering of image 1 only, not its
   subject or layout." To lay the options out on a Lookdev canvas instead, add `canvas: {name: "lookdev"}` and
   drop the style line from the prompt (the card's run adds it).
2. Check against the file: landmarks present, on the side the Layout says, scale right, empty. Save the pick
   with `copy_attachment {attachment_id, path: "world/places/old-bell-quay/sheet.png"}` and update the file if
   the image settled something the text didn't. Ask the user to approve it (`character-design` §3); an approved
   sheet is final: variants and alternatives go to new files, never over it.

## 3. Variants and plates

Every variant passes the sheet as image 1, states the change first, then what stays:

```
Change only the time to night: sodium street lamps on the right side, wet granite reflecting them, the rest
of the quay in deep blue shadow. Keep the same place as image 1: same camera position, framing, buildings,
landmarks and their positions. Deserted. <Style line>
```

- **Reverse and side angles** (`refs/reverse.png`): "The same place as image 1, seen from <the opposite
  position>: now <what Layout says is there>." Make the reverse as soon as a scene has a dialogue across it.
- **Times of day and weather** (`refs/night.png`, `refs/storm.png`): only those the scripts use.
- **Story states**: a damaged or changed version gets its own ref and a dated line in the file. Never overwrite
  the sheet with a later state; earlier episodes still use it.
- **Shot plates** (an empty view at a shot's exact framing, to put characters into for a keyframe) are image
  cards in the shot frame on the episode canvas, wired `reference` from this place's Entity node or the
  variant's image; their prompt leaves out the style line like any card's (`image-prompting`).

Save each with `path: "world/places/<id>/refs/<name>.png"`, at the sheet's aspect, and add it to `refs:` as a
named reference: a table with the `name` a shot note will use (`Reverse`, `Night`, `View from above`) and what it
`defines` (the angle, or the light alone: "the night light only; not its framing"). Its id is the file's
name without the extension (`refs/reverse.png`: `reverse`), so name the file for the view; that is what
`@places/old-bell-quay#reverse` mentions.

**Several views that agree: one video of held stills.** A room, a cockpit or a set a dialogue is covered in
needs four or five views (each wall, from above, the seat with someone in it), and images generated one by one
disagree about what is where. A video model keeps one space across the shots of a single clip, so ask it for
stills and cut them out:

```
Five shots, 20 seconds, 16:9. Hard cuts at 4, 8, 12 and 16 seconds. Each shot is a held still frame: the camera
is locked and nothing in the frame moves for its full 4 seconds. No voice, no music, no subtitles.
Image 1 defines the place: <what to take>. Not its framing. <Image 2 defines the person seated in shots 3–5.>
THE SPACE (identical in all shots): <everything in it and where, by the Layout's side names>
SHOT 1, 0 to 4 s - <view>: the camera sits <where>, at <height>, looking <where>. <What is in frame, left to
right.> <Field of view in degrees.>
SHOT 2, 4 to 8 s - <the reverse> …
LIGHT: <the sources; the brightest point of each frame>
AT THE END: the space was identical in all five shots; each shot stayed completely still.
```

Then one frame from the middle of each shot (`-ss 2`, `6`, `10`, …), `read` each, and save the good ones:

```sh
f=<the take>.mp4; d=world/places/old-bell-quay/refs
for i in 1 2 3 4 5; do ffmpeg -hide_banner -loglevel error -y -ss $((i*4-2)) -i "$f" -frames:v 1 $d/view-$i.png; done
```

Rename them for what they show, as the file's name is the id (`refs/from-the-door.png`: `#from-the-door`), and list each in `refs:` as a named reference (`name:
View from the door`, `defines:` the view it fixes). The same method gives a vehicle's interior (`prop-design` §3).

## 4. A layout map

When continuity needs it (a chase, a dialogue covered from many angles, a place visited across episodes), make a
plan view for you and the user:

```
Simple top-down map of <name>, clean flat illustration on a white background, each landmark labelled in
English: <landmarks and their positions from Layout>. An arrow marks the main camera position.
```

Pick a model that renders text well (see `image-prompting`), save it as `refs/layout.png`, and leave it out of
`refs:` (note it in the file: `Layout map: refs/layout.png, not a shot reference`). Image models would copy
the labels into shots. A **blocking image** for an action scene (who starts where) is a different thing: an
unlabelled wide or top view made as a card on the episode canvas and wired into the pass
(`action-choreography` §4).

## 5. In shots

- Mention the view matching the shot's time and angle after the characters (`@places/old-bell-quay#reverse`, or
  `@places/old-bell-quay` for the sheet): it wires the file into `reference` and the run says its job ("The Old
  Bell Quay, Reverse (image 2: take only the quay seen from the tower…)"). In a direct call, by hand: "image 1
  is Mara, image 2 is the place".
- Repeat the landmarks and their side in the shot prompt ("the leaning rusted bell tower behind her, frame
  left") so the model places them where the Layout says.
- `continuity` covers screen direction and keeping the camera on one side of the action across a scene.

## Pitfalls

- People, fog or a mood grade in the sheet: every shot inherits them.
- Moving the camera between variants: a variant is only a reference for the angle it shares.
- A generic place: if the description fits any harbour, add the landmarks that make it this one.
- Describing light by its effect ("her face glows") instead of its source and direction.

## Next

Characters → `character-design`. Vehicles, machines and what sits in a set → `prop-design`. The scenes set
here → `series-writing`, then `shot-listing`. Ambient sound for the place → `sound-design`.
