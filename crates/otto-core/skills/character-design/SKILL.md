---
name: character-design
description: Creating or changing a character in a Studio - the entity file in world/characters/<id>/ (prompt-ready appearance with identity anchors, wardrobe, movement), then the asset pipeline: a three-view sheet in one image, combining the face of one option with the outfit of another, the fixing pass (edits, or the user painting over it by hand), state references for what the sheet does not show, a 360° video sheet with a spoken line for video scenes, and voice casting. Load for "create a character", Generate sheet / Generate video sheet / Cast voice requests from the World view, a new outfit or a changed look, or choosing a voice.
---

# Character design

A character stays the same person across a series because every generation carries the same reference, not
because the prompt describes the face well: text descriptions dilute across many generations and models fall
back to a generic face. So a character is built once, as an asset, before any shot uses them:

**file → sheet (three views) → combine the best parts → fix → state references → voice → video sheet.**

Stop where the project stops needing more: a walk-on needs the file and a sheet; a lead who talks and moves
through video scenes needs all of it. Load `image-prompting` for prompt shape, model choice and multi-reference
wording; this skill covers what is specific to characters.

## 1. The file

`world/characters/<id>/<id>.md`, id a lowercase slug (`mara`, `warden-oske`):

```markdown
---
name: Mara Venn
sheet: sheet.png
refs:
  - refs/face.png
  - refs/front.png
  - file: refs/hood-down.png
    name: Hood down
    defines: her hair once the hood is down; not her face
  - file: refs/outfit-drysuit.png
    name: Drysuit
    defines: the diving outfit, head to boots
  - refs/mara-360.mp4
links: [props/compass]
voice: elevenlabs/<voice_id>
tags: [lead, diver]
summary: Salvage diver, early 30s, cropped uneven black hair, pale scar through the left eyebrow, father's oversized brown oilskin coat.
---
## Appearance
Woman, early 30s, 1.68 m, wiry swimmer's build. Long face, high cheekbones, olive weathered skin, freckles
across the nose. Dark brown deep-set eyes under heavy straight brows; a pale scar cuts vertically through the
left eyebrow. Cropped uneven black hair, usually damp. Chapped lips, a slight overbite.

## Wardrobe
- Default: father's brown oilskin coat, two sizes too big, sleeves rolled; grey wool jumper; black canvas
  trousers; yellow rubber boots; a brass compass on a cord around her neck.
- Diving: black neoprene drysuit with orange shoulder panels (refs/outfit-drysuit.png).

## Personality
Stubborn, dry, kind to children and animals, curt with authority. Hides fear by working.

## Mannerisms
Rubs the brow scar when thinking. Stands with her weight on one leg. Never takes the coat off indoors.
Speaks in short sentences; says "right" to end a conversation.

## Movement
Resting face: a guarded, level stare, mouth closed. Moves economically, hands always busy. In a fight stands
side-on with open hands, waits, answers rather than starts; gives ground one step when hit, never two.

## Backstory
Only what shapes behaviour or looks.
```

- **Appearance is the sheet's prompt subject.** Identity first, in this order: sex and age, build, face shape,
  brows and eyes, nose and lips, skin, hair, marks. Concrete nouns, no "beautiful", "striking", "mysterious".
- **One instantly recognisable anchor at least** (a scar, a mole, a haircut, a piece of jewellery) plus a
  silhouette anchor that survives a wide shot (the oversized coat, the yellow boots). Name them the same way in
  every file, script and prompt.
- **Wardrobe is per outfit**, each named, each later with its ref. Changes over the story are dated lines
  (`From ep 4: coat torn at the left shoulder.`).
- **Movement** is how the body behaves, in words a video prompt can use: the resting face, how they stand
  and carry weight, their tempo, and for anyone who fights, how they stand, what they lead with and how they
  take a hit. `acting-direction` and `action-choreography` write from it. Add it when the first video of the
  character is planned.
- **`summary`** is the shortest prompt-ready version (under ~25 words), anchors included.
- **List `refs` explicitly**: only what should be passed as a reference. Rejected or archived images stay out.
  An entry is a path (named by its file: `face`), or a table with a `name` and what it `defines` when it is a
  state, an outfit or anything that must give only one thing to a shot. Its id is the file's name without
  its extension, so name the file for what it is (`refs/hood-down.png`: `hood-down`):
  `@characters/mara#hood-down` mentions it on a card.
- **`links`** name the props that are theirs (`props/compass`); a prop made for them is linked from either file.
- **Write the temperament into the body.** Two lines of who they are, as posture and face ("stands like someone
  waiting for the next task: shoulders level, hands quiet, mouth a flat line; tiredness under the eyes but no
  slump") give the sheet a person rather than a mannequin, and it is the resting state every shot starts from.

## 2. The sheet: three views in one image

One wide image (`aspect_ratio: "16:9"`) on a plain white background: **a full-length front view on the left, a
full-length back view in the middle, and a large head-and-shoulders portrait on the right**, the portrait at
about twice the scale of the figures. The front and back fix the design from both sides, the portrait carries
the face at a size models can read, and one image keeps the three in agreement, which three separate
generations don't. It is a file made with a direct call, not a card, so you set the aspect and write the medium
line yourself.

The prompt is long and sectioned. Its length is facts: every garment and item with its colour, material and
where it sits.

```
A three-view character sheet of one <who, in five words> on a plain white background: full-length front view
on the left, full-length back view in the middle, and a large head-and-shoulders portrait on the right. All
three are the same single design: identical clothes and items in the same positions in every view. The sheet
holds these three views and nothing else.

THE CHARACTER, shown through posture and face:
<Age and temperament in one line. How that sits in the body: shoulders, chin, hands, stance. The resting
expression: eyes, mouth. From Personality and Movement.>

THE BUILD:
<Height in heads, proportions, how stylised: from Appearance and the medium.>

THE HEAD / THE UPPER BODY / THE LOWER BODY / FEET:   (one block per zone that has anything on it)
<What is worn there, item by item: shape, colour, material, where it attaches, which side. A zone that is
plain says so as a state: "the torso stays clean and narrow, nothing hangs from it".>

THE THREE VIEWS:
- FRONT (left, full length): <stance, arms, where the gaze goes; what reads from this side>.
- BACK (centre, full length): seen from directly behind; <what is only visible from here>.
- PORTRAIT (right, head and shoulders, about twice the scale): three-quarter turn toward the viewer; <what of
  the head and collar is in the crop>. The look that carries the character: <the resting expression>.

THE FACE:  <face shape, eyes, brows, nose, mouth, skin, marks with their side: the Appearance words>
THE HAIR:  <cut, length, colour in light and in shadow>
COLOUR PALETTE:  <the five or six colours and where each sits; how it is shaded>
COMPOSITION:
Wide horizontal sheet, plain pure white background, the two figures aligned at head height and on one foot
line, the enlarged portrait on the right, even spacing, no ground shadow.
<Medium line from style.md>
```

Why it is shaped this way:
- **"The same single design" is said outright**, and the views say what each one shows. Otherwise the model
  restyles the outfit between views or adds detail drawings, spare parts and labels around the figures.
- **A resting expression and stance, described**, never "neutral expression" (which removes what makes the face
  this person's) and never a scene's emotion: grief, fear or a smile on the sheet turns up on every shot made
  from it.
- **White background, even spacing, no ground shadow.** Whatever background and light the sheet carries, every
  shot inherits. A photoreal medium wants soft directional light that shows the bones of the face (soft key
  from the upper left, gentle fill); a painted or drawn medium wants the shading of the medium itself (flat
  fills, simple light and shadow planes).
- **Only the medium line, not the full style line.** Palette, grade, atmosphere and scene lighting belong to
  shots and plates; on a sheet they bake into every shot.
- **Something hidden is described by what shows.** A visor stowed inside a helmet: "only a narrow dark slot along
  the brim and one small red button beside the ear cup". Its other state gets its own reference (§4).

Calls:
1. Options first: `generate_image {prompt, aspect_ratio: "16:9", n: 3}`, no `canvas` (a card's run would
   append the style line, which a sheet must not carry). If the user wants them on a Lookdev canvas
   (`canvas_create {title: "Lookdev"}` once), add image cards with `canvas_edit` with the project style off
   (`options.style: false`) and the 16:9 aspect in their options (names per the tool's description), and
   `canvas_run` them. To carry a non-photoreal rendering style, add the style frame as a reference and say
   what to take from it:
   `references: ["world/style/frame-01.png"]` with "Render in the drawing style of image 1 only: its line work
   and shading. Not its scene, light or colours." (On a Lookdev card, mention the style frame's asset node
   instead, `@#<its id>`: that wires it and numbers it when the card runs.)
2. Check each against the file, view by view: anchors present and on the correct side in the front view *and*
   mirrored correctly in the back view, the same items in all three, age, build, one person, nothing invented
   (a hat, a bag, a logo, lettering). Say in a line or two what each option gets right: often one has the face
   and another the outfit. That is normal; §3 joins them.
3. Save the pick: `copy_attachment {attachment_id, path: "world/characters/mara/sheet.png"}` (`overwrite: true`
   when replacing). When the user doesn't need to choose, generate straight into it with `path` and `n: 1`.
4. Cut single views out of the approved sheet for the models and shots that want one figure per image (a
   keyframe model that copies the sheet's three-figure layout, a close-up that needs only the face). Read the
   sheet's size with `ffprobe` first and adjust the thirds to where the figures actually stand:
   ```sh
   d=world/characters/mara
   ffmpeg -hide_banner -loglevel error -y -i $d/sheet.png -vf "crop=iw*0.34:ih:iw*0.66:0" $d/refs/face.png
   ffmpeg -hide_banner -loglevel error -y -i $d/sheet.png -vf "crop=iw*0.33:ih:0:0" $d/refs/front.png
   ```
   `read` each crop, then add them to `refs:`.
5. If the image settled something the text didn't (the hair came out shorter and the user likes it), edit the
   file to match. The sheet and the file never disagree.

## 3. Combine, then fix

**Combine.** When the face of one option and the outfit of another are the ones to keep, join them with an
image model that edits from several references (`image-prompting`), both options passed as `references`:

```
Create one consistent character sheet for <name> by combining the two references.

IMAGE 1 - HEAD: take her head from image 1: the face, its proportions, skin, hair and <headgear>. Use its
large portrait as the main guide to her identity and expression.
IMAGE 2 - BODY AND OUTFIT: take her body proportions and clothing from image 2: <each garment and item>.
Keep the outfit's shapes, colour blocking and where each item sits.
COMBINE: fit the head from image 1 onto the body from image 2. Match head scale, neck, perspective, skin tone
and light. Turn the head to suit each view while keeping the same face.
STYLE: one consistent <medium> across the whole character, so head and body read as one illustration.
LAYOUT: white background, 16:9 sheet: full-body front view on the left, full-body back view in the middle,
large portrait on the right. The same design in all three.
```

**Fix.** A generated sheet is a base, not the final design. Before it becomes `sheet.png`, look for what is
still wrong (a strap that changes between views, a muddled buckle, a hand, lettering, a detail the user never
liked) and deal with it now: every shot inherits what the sheet carries.

- A small change: an edit with the sheet as the reference, the change first, then one sentence of what stays
  ("Keep the same face, views, layout, background and light."). GPT Image also takes `markup` to point at a
  region. After two or three edits features drift: go back to the last good version, don't chain further.
- **The user's own hand is the best fix**, and the workflow expects it. Tell them what you would touch up and
  that the entity page opens the file in their image editor (right-click the sheet: Open in default app, or
  Reveal in Finder); they paint over it and save in place. Then `read` the saved sheet, tell them what you see
  changed, and bring the file's text in line with it.
- When the sheet is right, ask the user to **approve** it (the World view, or they tell you: then add
  `approved: <date -u +%Y-%m-%dT%H:%M:%SZ>` to the frontmatter). `studio_read`'s Production block shows each
  sheet's approval; a sheet changed since shows `CHANGED since approved` and needs approving again. An approved
  sheet, or one the user fixed by hand, is final: never regenerate over it; an alternative goes to a new file
  (`sheet-alt.png`) and the user chooses. A change they ask for starts from it as the reference, after keeping
  a copy: `shell` `mkdir -p …/refs/archive && cp sheet.png refs/archive/sheet-v1.png` (not in `refs:`).
- A big change (a new age, a new build): rewrite the appearance first, then a new sheet, then every ref again
  from it. Cards wired from the old sheet show Out of date; say which shots and offer to re-run them
  (`continuity`).

## 4. State references

The sheet shows one state. Anything the script needs that it does not show gets **one image that defines only
that thing**: the hair under a helmet she takes off, the visor switched on, the coat off, the arm in a sling.
Without it the model invents the missing part differently in every shot.

- Pass the sheet as image 1 ("the same person as image 1") and describe only the state. Same white background.
- **Keep everything else out of the image**, so nothing but the state can be taken from it: frame it tightly on
  the thing, and for a hair or headgear reference turn the head to profile or cover the face (a flat patch
  drawn over it with `ffmpeg … drawbox`, or ask the user to), so the face only ever comes from the sheet.
- Save as `refs/<state>.png` (`refs/hood-down.png`; the file's name is the id) and record it as a named reference: a table in `refs:` with its `name`
  (`Hood down`) and what it `defines` (`her hair once the hood is down; not her face`). Note in the file when
  the state applies (`From ep 2 sc 4: hood down.`).
- In a prompt it gets exactly that job. On a card, mention it (`@characters/mara#hood-down`): the run writes
  "Mara Venn, Hood down (image 3: take only her hair once the hood is down; not her face)". In a direct call,
  by hand: "Image 3 defines only her hair once the hood is down. Take only the hair from it; not its face,
  light, clothes or background."
- **Outfits** are states of the whole body (`refs/outfit-<name>.png`, a three-view or a front view), named
  references the same way: "Same person as image 1; change only the clothes to <outfit in full>." Keep her
  hair and anchors stated.
- **Expressions**: rarely worth a reference. The acting comes from what the character is doing in the shot
  (`acting-direction`), and an expression reference puts that expression on every frame. Make one
  (`refs/expr-<emotion>.png`, the emotion described physically) only for a face a model can't otherwise reach.

## 5. Voice

Cast the voice before the video sheet (§6 speaks a line in it) and before any dialogue is recorded.

1. `list_media_models {kind: "audio"}` lists the voices with a description of each.
2. Pick 2–3 that fit the file (age, accent, temperament); say why in a line.
3. Write a sample line in their register (8–15 words, with an emotion the script needs) and generate each:
   `generate_audio {type: "speech", text, provider: "elevenlabs", voice: "<voice_id>"}`.
4. Once picked: `copy_attachment` it to `world/characters/mara/voice-sample.mp3` and set `voice:
   elevenlabs/<voice_id>` in the frontmatter. `sound-design` reads it as `provider/voice`.

## 6. The video sheet

A still sheet shows two or three angles. Scenes look at a character in profile, over the shoulder, from behind
and in motion, and a video model guesses each of those again. A **video sheet** is a short clip that shows the
character from every side, moving, and saying one line: later video prompts take the face, the build, the way
they move and the voice from it. Make one for every character who carries video scenes, on a video model that
takes reference images and makes sound (`list_media_models {kind: "video"}`, then `media_model_schema`).

```
<N> shots, <12–25> seconds, square frame. <Cut times.> No other cuts.

SHOT 1, 0 to 4 s. A close portrait from the collarbones up, her whole head and anything worn on it inside the
frame with room above, camera still, at her eye level. She is silent. <One small change of expression, in
order: "her eyebrows lift first, then a soft smile arrives.">

SHOT 2, 4 to 15 s. Full figure, head to feet, standing in the centre with room above and below, in her stance
from the sheet.
THE CAMERA. It orbits her exactly once, clockwise, a full 360 degrees, front view back to front view, smooth
and at a constant speed, at chest height. It keeps her centred and the same size the whole way round, and keeps
travelling whatever she is doing. She never turns to follow it.
WHAT SHE DOES, tied to where the camera is:
- From the front round to her side: she says one line, "<Hi. My name is Mara.>" Nothing else moves.
- From her side round to her back: she stands completely still.
- From her back round to her other side: <one action that shows how she moves or changes a state, e.g. pushes
  the hood back>. Then she holds still.
- The last quarter back to the front: her stance from the sheet, arriving as the camera reaches the front.
Each state arrives quickly and is then held completely still until the next. The line and the action never
happen at the same time.

RUNNING THROUGHOUT: slow breathing in the chest and shoulders, a natural blink now and then. Her feet stay on
the same two spots and never slide.
SET: a seamless warm off-white backdrop. One person exists in this video.
REFERENCES: Image 1 defines her face, build and outfit exactly, and her painted look. Not its poses, its
layout or its white background. <Image 2 defines only …>
AUDIO: her voice (<voice in a few words>), speaking only that one line, once, in shot 2; shot 1 and the rest
of shot 2 are quiet room tone. No music, no subtitles.
LIGHT: one soft key from the front-left, fixed to the set, so the shaded side travels across her as the camera
goes round, and an even soft fill from all sides, so her back and far side stay clearly readable, never dark.
<Style: the medium, and for an animated show how it moves (`animation-direction`).>
AT THE END: the camera completed one full turn and is back at the front. The line was spoken once. <Each
state changed once and stayed.> She stood on the same two spots throughout.
```

- **One orbit per state.** When the script has her with and without something (a helmet, a coat), add a short
  shot in which it changes and a second orbit in the new state, wiring that state's reference (§4).
- **The spoken line is the voice reference.** Wire `voice-sample.mp3` (or a Speech card of the line in her
  voice) into the model's audio input so she says it in the cast voice, and say so in AUDIO.
- **The closing recap, in the past tense**, restates what happened once and what never changed. It is the last
  thing the model reads and it holds counts ("the visor turned off once and stayed off").
- Make it with `path: "world/characters/mara/refs/mara-360.mp4"` and the sheet (and state refs) as references,
  or as a card on the Lookdev canvas. The file name carries her name: it is how the clip is labelled wherever it
  is wired.
- **Check it**: you don't see video. Grab frames (`continuity` §5; 2 per second, `tile=8x3`) and `read` them:
  one person, the same outfit from every side, back view matching the sheet's, feet planted, the orbit complete.
  A wrong back or a drifting face here is copied into every scene, so re-run rather than accept it.
- Add it to `refs:`. **In a scene** it is an asset node of the file on the episode canvas, wired into the video
  card's `video` port, with its role and limits in the prompt: "`@#<its node>` is Mara: her face, build,
  outfit, the way she moves and her voice. Ignore its studio, its orbit and what she does in it." A scene whose
  state differs from the clip says so as an override, in capitals, with what is there instead: "OVERRIDE: in
  this scene her hood is down for every shot; her whole head and hair are visible, as in
  `@characters/mara#hood-down`."
- Stills (keyframes, plates) and models that take no reference video keep using the image sheet and its crops.

## Pitfalls

- Paraphrasing the appearance between prompts: copy it.
- Mood lighting, coloured backgrounds, props or action poses on a sheet: every shot inherits them.
- Two characters on one sheet, or a sheet with labels, callouts or spare parts drawn in.
- Accepting a nearly-right sheet: fix it now (§3), or the flaw is in every shot.
- Refs generated without the sheet as a reference: they are a different person.
- A reference that shows more than its job (a hair reference with a clear face): the model takes the rest too.
- A peak expression on the sheet or in the video sheet.
- A model that takes no references (Higgsfield Soul through these tools) for anything after the first sheet.

## Next

Props, vehicles and machines → `prop-design`. Places → `location-design`. The script → `series-writing`. Using
the character in shots → `shot-listing`, `image-prompting` and `continuity`; scenes of talk →
`dialogue-scenes`; their lines → `sound-design`.
