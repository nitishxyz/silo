---
name: prop-design
description: Creating a recurring prop, vehicle, machine, robot or weapon in a Studio - the entity file in world/props/<id>/ (silhouette, named parts with counts, how it works, scale), a two-view sheet, the fixing pass, interior and detail views that agree with each other, a 360° video sheet that shows its moving parts working, a mechanism reference for one function, and a placement image for where a small prop sits in a set. Load for "design the ship / car / robot / sword", Generate sheet and Generate video sheet requests on a prop, a set a character sits inside, or when a prop changes shape or detail between shots.
---

# Prop design

A prop that appears in more than one shot is an asset like a character: made once, fixed, and passed as a
reference everywhere. Machines are harder than people for a model to keep (how many legs, which side the hatch
is on, what the back looks like), so the file counts things and the references show every side and every
moving part. The order:

**file → sheet (two views) → fix → interior and detail views → video sheet → mechanism references.**

A hand prop (a compass, a letter) stops after the sheet. A vehicle a scene happens inside needs all of it.
Something that acts and has a will of its own (a creature, an android) is a character: `character-design`.
Load `image-prompting` for prompt shape and model choice.

## 1. The file

`world/props/<id>/<id>.md`:

```markdown
---
name: The Tern
sheet: sheet.png
refs:
  - file: refs/seat.png
    name: Seat
    defines: the wheelhouse seat and controls, seen from the door
  - file: refs/footwell.png
    name: Footwell
  - refs/tern-360.mp4
  - file: refs/tern-winch.mp4
    name: Winch working
links: [characters/mara]
tags: [vehicle, mara]
summary: Mara's salvage skiff: blunt grey hull six metres long, orange wheelhouse the size of a phone box, one stern winch with a brass drum.
---
## Shape
Reads as a floating toolbox: a blunt rectangular hull, low in the water, with one upright box (the
wheelhouse) set far back and a single arm (the winch) over the stern.

## Parts
- Hull: riveted steel, slate grey, a rust line at the water, one white stripe along each side.
- Wheelhouse: exactly one door, on the port side; three square windows (front, port, starboard); orange.
- Winch: one arm, one brass drum, a hand crank on its right; grey rope, never chain.
- Count of masts: zero. Count of sails: zero. One round lamp on the wheelhouse roof.

## How it works
The door slides back, it does not swing. The winch arm folds forward flat onto the deck when stowed and
stands at 45 degrees when working; the drum turns clockwise to haul.

## Scale
The wheelhouse roof is level with Mara's shoulder when she stands on deck. Two people fill the deck.

## Wear and sound
Dents along the starboard bow. A two-stroke engine that coughs on starting; the winch ratchets.
```

- **Shape first, in one image the model already has** ("a beetle's shell on four legs", "a floating toolbox").
  It decides the silhouette before any detail does.
- **Counts, both ways.** How many of each part ("exactly four legs, no more and no fewer"), and zero for what a
  model will add by habit ("Count of weapons: zero"). A machine with nothing ruled out grows guns and antennae.
- **How it works** names every moving part, its hinge and its two states. Video prompts copy these words.
- **Scale against a person**, or it changes size from shot to shot.
- **`links` names its owner** (`characters/mara`) when it is made for a character: one mace goes with Bhima,
  another with Duryodhana, and the link says which. Write it once, here or in the character's file; it reads
  from both ends. `refs` entries that give one thing (an interior view, a detail) are named references (a table
  with `name` and `defines`, `character-design` §1).

## 2. The sheet: two views

One wide image (`16:9`): **the same object twice at the same scale, a three-quarter front view on the left and
a straight rear view on the right**, on a plain off-white background, all feet or the base on one ground line.
The back is where models improvise most, so it is on the sheet from the start. A hand prop takes a front and a
back, or a single three-quarter view with a hand for scale.

```
A two-view concept sheet on a plain warm off-white background: the same <thing> shown twice at the same
scale, a three-quarter front-left view on the left and a straight rear view on the right. <The counts.>
The sheet holds these two views and nothing else.

OVERALL SHAPE: <the Shape paragraph>
<A block per part: what it is, shape, colour, material, where it joins, which side.>
REAR VIEW: <what is only visible from behind, left to right>
<Moving parts in the state the sheet shows: "the canopy flipped fully up and open, held by one curved arm".>
COLOUR: <the dominant colour and where; the structure's colour; small accents only, each named>
LIGHT AND COMPOSITION: soft even light from the upper front, simple shading, a soft grey shadow under each
view. Both views at the same scale and height, evenly spaced, the background completely empty.
<Medium line from style.md>
```

Options with `generate_image {prompt, aspect_ratio: "16:9", n: 3}`; check each against the counts; save the pick
to `world/props/<id>/sheet.png`. Then the same **fixing pass** as a character's (`character-design` §3): take
off invented detail, add the wear, correct the count, by an edit or by the user painting over it. Then ask the
user to approve it (`character-design` §3). An approved or hand-fixed sheet is final and never regenerated over;
an alternative goes to a new file.

## 3. Interior and detail views that agree

A cockpit, a cabin or a room-sized prop needs several views (the seat from the front, from above, the
footwell, the same with the character seated). Generated as separate images they disagree with each other.
Make them as **one video of held stills** and cut the frames out: `location-design` §3 has the method and the
prompt. Save each frame as `refs/<view>.png` (the file's name without its extension is the id: `refs/seat.png`,
`@props/tern#seat`) and list it in `refs:` as a named reference: its `name` (`Seat`) and what it `defines`.

Include a view with the character in place when a scene sits there: it settles where the knees, hands and head
go, which no exterior sheet shows.

## 4. The video sheet

Like a character's (`character-design` §6), with one difference: **it shows the thing working.** One orbit,
and on the way round each moving part goes through its states, one at a time.

```
Two shots, 12 seconds, square frame. One hard cut at 4 seconds. No voice, no music, no subtitles.

SHOT 1, 0 to 4 s. Close on <the part that opens>, camera still, slightly below it. <It opens: unlocks with a
small jolt, swings on its hinge in one arc, settles.> As it opens the inside is revealed as in image 2: <list>.

SHOT 2, 4 to 12 s. The whole <thing> in the centre of the frame with room around it, in its resting state.
THE CAMERA. It orbits exactly once, clockwise, a full 360 degrees, front view back to front view, smooth and at
a constant speed, at the height of its body. It keeps the <thing> centred and the same size the whole way
round. The <thing> never turns to follow it.
WHAT IT DOES, tied to where the camera is:
- From the front round to its side: completely still.
- From its side round to its back: <part A closes, locks>. Holds still.
- From its back round to its other side: <part B moves to its second state>. Holds still.
- The last quarter back to the front: <back to resting>, arriving as the camera reaches the front.
Each state arrives quickly and is then held. Two parts never move at the same time. Its base stays on the same
spots and never slides.

RUNNING THROUGHOUT: <lights that glow steadily; highlights sliding across glass as the view changes>. The
surface does not change shape; only the light moves across it.
SET: a white void; the ground a flat white plane with a thin light-grey grid fading to a soft horizon. A soft
grey shadow under it. One <thing> exists in this video and it is empty.
REFERENCES: Image 1 defines it exactly, front and back: <the parts>. Not its layout or its background.
Image 2 defines only the interior: <list>. Take nothing else from it.
<One correction, if the sheet has a known flaw: "its back view is too clean: carry the worn speckled paint of
the front view over every side.">
AUDIO: <a sound per movement, in order>. Quiet room tone.
LIGHT: soft key from the front-left, fixed to the set, so the shaded side travels across it, and an even soft
fill from all sides, so its back stays clearly readable, never dark.
<Style>
AT THE END: one full turn, back at the front. <Each part moved once and ended in its state.>
```

- **A grid on the ground** gives the model the scale and the perspective a blank void doesn't.
- **A correction line** is the place to repair a flaw of the sheet without remaking it.
- Save as `refs/<id>-360.mp4` (its reference id: `<id>-360`), check it by frames (`continuity` §5), add to `refs:`.

## 5. Mechanism references

A function the story shows more than once (a drill deploying, a door sealing, a weapon reloading) gets its own
short clip, so it works the same way each time:

```
One shot, <8–12> seconds, no cuts. Sound effects only.
Video 1 is the <thing>. This is a new video, not a continuation. Take its design, parts, materials, colours and
look exactly. Not its camera move, its orbit or its framing.
CAMERA: locked, <where it stands, how far, looking where>. It does not move for the whole shot.
THE MECHANISM: <the parts involved, built from what, how many: "exactly two">
ACTION, timed:
0–1.5 s: at rest. 1.5–3.5 s: <it prepares, then deploys, parts following one another>. 3.5–8 s: <it works;
what the material does; what the rest of the body does in response>. 8–10 s: <it finishes>. 10–12 s: <it
stows; settles; holds still>.
<Set, light, audio, style as in the video sheet.>
AT THE END: <what came out, did its work once and went back>.
```

Wire the video sheet (an asset node of it, or a direct call's reference video) as Video 1. Save as
`refs/<id>-<function>.mp4`. Scenes that show the function wire this clip and say: "the <function> works as in
Video 2; not its camera or its set".

## 6. In shots

- On a card, mention the prop (`@props/tern`) to wire its sheet, or one of its named references
  (`@props/tern#seat`) for a view the shot needs, with its job said for you; for video scenes also wire an
  asset node of the video sheet into the `video` port and give it its job and limits (`video-prompting` §3).
- **The owner's prop, not another one like it.** A shot with Bhima and a mace mentions the mace his file links
  (`studio_read` lists each entity's `links`).
- **State which state it is in** in every shot (door open or shut, arm stowed or raised) with the words of
  `## How it works`.
- **A placement image** for a small prop that must sit in the same spot across a scene (a photo taped inside a
  cockpit, a mug on a console): one image of the set with the prop in place, made as an image card on the
  episode canvas from the set view and the prop. In prompts its job is the place only: "Image 8 is where the
  photo sits: on the padded wall at her head height, a hand's width from the glass. Not its light or framing."
  Add the count: "Count of photos: one."
- Damage and changes are dated lines in the file and, when they show, a new ref. The sheet stays the original.

## Pitfalls

- No rear view: the back is different in every shot.
- Parts without counts, and nothing ruled out.
- Interior views generated one image at a time.
- A video sheet where two parts move at once, or the thing walks: the orbit stops being readable.
- Using a mechanism clip's camera and set along with its mechanism: say what stays behind.

## Next

Characters → `character-design`. Sets and places → `location-design`. Wiring references into video →
`video-prompting`, `dialogue-scenes`, `continuity`.
