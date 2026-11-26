---
name: action-choreography
description: Fights, duels, chases, falls, stunts and battles in a Studio - choreographing in timed beats, writing every strike as body geometry with the target's reaction, positive locks instead of prohibitions, the place's sides and a blocking image, planning continuous action as a multi-shot pass (one generation, several timed shots), then re-coverage from new camera angles and an extend for the finish. Load when a script has physical action or contact, before the shot list and video cards of such a scene, and when action came out weightless, floaty, generic, slow, or with hits that miss and damage that heals.
---

# Action choreography

Left alone, a video model makes "movie fighting": wide wind-ups, endless combinations, hits that stop short of
the body, damage that is gone in the next second. And two clips generated separately each guess pose and tempo
again, so a fight cut from one-beat clips stutters. Two things fix this: design the action on paper as beats
with consequences, and generate continuous action as a **pass**, one generation holding several timed shots,
instead of shot by shot.

Load `video-prompting` for modes and per-model practice, `continuity` for references and chaining. For an
animated show also load `animation-direction` (held poses, drawn impacts, inserts). Faces and lines inside an
action scene are their own shots: `acting-direction`.

## 1. What the action is for

Before any beat, write three lines at the top of the choreography file (§2):

- **Who wins, and what it costs them.** A fight nobody pays for is noise.
- **The turn**: the moment the upper hand changes or the plan fails (the scene check in `series-writing`:
  every sequence turns at least once). The beats before it set it up; the beats after pay it off.
- **What is different afterwards** that the next scene must show: a wound, a lost weapon, a broken place, who
  knows what. It goes into the world files in the same turn (`continuity`).

Shape: **stillness → provocation → exchange → the turn → the finish → stillness.** Two to four exchanges tell
more than twelve. Stillness before and after is what makes the burst read as fast.

Each fighter moves like themselves. Add `## Movement` to the character file (`character-design`) once, and
write every beat from it: how they stand, what they lead with, their tempo, how they take a hit.

```markdown
## Movement
Stands side-on, weight low, hands open at chest height. Waits; answers an attack rather than starting one.
Short straight movements, then goes completely still. Takes a hit by giving ground one step, never falls back
in a straight line twice.
```

Build the pair on contrast (economical against wild, heavy against quick): it tells the model which body is
which and tells the viewer who is winning.

## 2. Choreograph in beats

A beat is one movement and its consequence, 1–2 s of screen time:
**position → action (which limb, along which path, to which target) → the other body's reaction → new position.**

Budget the clip before writing: 8 s holds 4–6 beats, 15 s holds 8–10, 30 s about 15, stillness included. More
than that and the model drops beats or blurs them together.

Write it as a document on the episode canvas, `canvases/<ep>/fight-sc<N>.md`, and pin it as a document card in
the scene frame (`add_node` of an asset whose `path` is the file), so the user can read and change the fight
before anything renders:

```markdown
# Fight · Scene 3 · Mara and Warden Oske on the quay (15 s)
Wins: Mara, by using the place. Cost: the compass cord snaps; she loses it in the water.
Turn: beat 5, Oske has her pinned at the winch and the chain she grabs runs free.
After: Oske's lamp broken on the stones; Mara without the compass (from ep 1 sc 3).
Sides, from the quay steps looking out to sea: tower ahead-left, Guild side right, water at the far edge.

| # | Time | Starts | Action | Reaction | Ends | Force |
|---|---|---|---|---|---|---|
| 1 | 0.0–1.5 | Mara at the winch, back turned; Oske on the steps | Oske crosses the quay in four fast steps | Mara turns side-on, hands open | two paces apart, still | - |
| 2 | 1.5–3.5 | facing, still | Oske swings the lamp pole wide from his right | Mara steps inside the arc; the pole passes behind her shoulder | chest to chest | light |
| 3 | 3.5–5.0 | chest to chest | Mara's open palm drives up under his chin | his head snaps back, he gives two steps | Oske at the tower side | heavy |
| … |
```

**Force** per hit: `light` (it stings, the head turns, feet stay), `heavy` (the body is moved a step or more),
`finisher` (the body leaves its feet or goes down and stays). One finisher per pass at most; it needs clear
space behind the target and a named place to land ("into the coiled chain by the bollard").

## 3. Writing a beat for the model

These rules are about how models read text, and they hold on every video model:

- **Geometry, not terms.** Names of techniques, styles and schools (a roundhouse, a hip throw, any martial-arts
  word) carry no picture for the model and bring back generic movie fighting. Write which limb, from where it
  starts, the path it travels, which surface lands, where. Keep the term in the choreography file as a label
  for the user, out of the prompt.
- **The strike already travelling.** Describe the movement in flight and what it does. Leave out preparation
  (draws back, winds up, gets ready): the model spends the beat on it.
- **Every hit carries the other body's reaction, along the direction of the strike.** A straight hit sends the
  head back; one from the side turns it sideways; a hit to the body folds it around the spot; a push sends the
  whole body back along the push. Without the reaction the model makes the hit miss; with a reaction in the
  wrong direction it turns the move into an ordinary punch. The full shape of a hit: contact, the body goes,
  a stunned moment, recovery.
- **An unusual move needs three things**: something everyday the model has seen (like kicking a door shut,
  a backhand slap, hammering a nail), the surface that lands (the heel, the back of the fist, the forearm), and
  the reaction along its direction.
- **Stance in the first frame**, as a full description of the body, then the same short phrase every time the
  fighter returns to it ("the low side-on stance"), word for word.
- **Stillness is a state and has a length**: "goes completely still, feet planted, for a full second". Put the
  cuts on the stillness.
- **Weight**: say what the feet do (planted, skidding, heels digging), what the place does (dust jumps off the
  stones, the rail bends) and what is heard. Bodies that touch nothing but each other float.
- **Lean beats.** One to three sentences each. Piling layers of description onto a beat loses the one fact that
  mattered.
- **Framing follows the move**: footwork and kicks need the feet in frame; grips and holds read in a medium
  three-quarter view; straight lines read in profile; a throw needs the landing place in frame. Both bodies in
  frame at every contact.

**Positive locks instead of prohibitions.** Naming a thing in a "no …" brings it into the picture. State what
is there instead, and repeat each lock word for word:

| Don't write | Write |
|---|---|
| no wind-up | the first visible motion of the arm is the strike already travelling |
| no high kicks | every kick stays below belt height |
| no slow motion | real speed from the first frame to the last, one speed throughout |
| no blood, no wounds | skin unbroken, clean and dry; the hurt shows as swelling, sweat, dust and torn cloth |
| no extra people | two people on the quay, both in frame in every shot |
| no weapons | both bare-handed, hands open or closed, nothing held |
| no cuts inside a shot | each shot is one camera, held from its first second to its last |

`No subtitles.` `No music.` stay as they are: models follow those (`video-prompting` §3).

**Damage is one-way.** Whatever breaks, tears, falls or is dropped stays that way. List it with the second it
happens in the prompt's "what holds" section (§6) and in the scene's continuity note.

## 4. Geography and blocking

- **Name the place's sides once**, from one fixed camera position, with the place file's `## Layout` words
  (`location-design`): "from the quay steps looking out to sea: the tower is ahead-left, the Guild side is
  right, the water is the far edge". Every shot then says where the camera stands and where each body is by
  those names. Without this the model rebuilds the room at each cut.
- **A blocking image** fixes where the action starts: a wide or top view of the place with the fighters in
  their opening positions. Make it as an image card `S3 blocking` in the scene frame whose prompt mentions the
  place and the characters (`@places/old-bell-quay`, `@characters/mara`: that wires their sheets); no labels or
  arrows drawn in it (models copy them into shots). In the pass prompt give it one job: "@#<S3 blocking id> is
  the opening positions only; faces, light and rendering come from the other images."
- **Direction of travel** in a chase stays the same across cuts (`continuity` §3). Write the route as three
  landmarks in order, and give each shot one obstacle.

## 5. Plan the passes

`list_media_models {kind: "video"}` first: a pass needs a model that makes long clips (15–30 s), takes several
reference images, and for re-coverage takes a reference video. Read `media_model_schema {model, mode}` for its
limits.

On the canvas a pass is **one shot frame** holding its note and cards (`shot-listing` §3 has the pass note).
Titles: frame `Pass 3A · The quay fight`, cards `P3A video`, `P3A cover`, `P3A finish`.

1. **Base pass** (`P3A video`): reference-to-video from the sheets, the place and the blocking image. Its job
   is the action: plain coverage, wide and medium, both bodies in frame, few cuts. Run 2–3 takes and pick on
   the action alone (do the hits land, does the turn read, does damage stay), not on the camera.
2. **Re-coverage** (`P3A cover`, wired `video` ← `P3A video`, sheets still wired): the same action seen from
   new angles. The prompt says the source video is the action, second for second, and that only the camera is
   new; then lists the new shots with their cut times and which stretch of the source each one shows. Say what
   stays behind in the source: its angles, its cuts, its sound.
3. **The finish** (`P3A finish`, a video-extend wired `video` ← the picked clip): continue from the last
   frame. Keep the same positions, grip, light and wardrobe in the prompt so the join holds.
4. **Inserts**: a hand closing on a chain, eyes, a foot sliding. Ordinary short shots in their own shot
   frames, cut in by `assembly`.

Keep physics and performance apart. A pass asked for a readable fight and a readable face delivers neither:
the pass carries bodies wide and medium; the looks and the lines are singles made with `acting-direction`.

**A model that makes only short clips** (10 s or less): split the choreography at its stillness points into
passes of two or three beats, one shot frame each, and chain each pass's last frame into the next as
`first_frame` (`continuity` §4). Every pass still gets the full sides, stance phrases and locks.

## 6. The pass prompt

A pass prompt is long and sectioned; its length comes from facts (positions, times, reactions), never from
adjectives. The run appends the style line and the motion line, so it carries neither. Order:

```
<SCENE> - <place>. <What happens in one line>. <N> s, <K> shots, hard cuts at <times>.

REFERENCES
@characters/mara is Mara: her face, build and outfit. Its white background and flat light stay behind.
@characters/oske is Warden Oske: his face, build and uniform. Its background and pose stay behind.
@places/old-bell-quay is the Old Bell Quay: the place and its light. It is empty; the only people are Mara and Oske.
@#<S3 blocking id> is the opening positions only. Faces, light and rendering come from the other images.

THE PLACE AND ITS SIDES
<from the fixed position: what is ahead, left, right, behind; where the action runs from and to>

HOW THEY MOVE
Mara: <two lines from her Movement section>. Her stance: "<the stance phrase>".
Oske: <two lines>. His stance: "<the stance phrase>".

SHOTS
1. 0.0–3.5 s. Wide, profile, camera on the Guild side at chest height, both full figure.
   Picks up: Mara at the winch with her back to the steps, Oske on the top step.
   <beats 1–2, one to three sentences each>
   Leaves them: chest to chest beside the winch.
2. 3.5–7.0 s. Medium, over Mara's right shoulder, …
   Picks up: chest to chest beside the winch. <beats> Leaves them: …

LAST FRAME, 15.0 s
<who is where, in what state, what the camera sees>

WHAT HOLDS, in every frame
Two people on the quay, both in frame. Mara and Oske keep one face, one build and one outfit each.
From 9.0 s the lamp lies broken on the stones by the bollard and stays there. From 11.5 s the cord at Mara's
neck is snapped and the compass is gone. Skin unbroken, clean and dry. Real speed, one speed throughout.

SOUND
Ambience: water against stone, a chain creaking. Impacts: <one sound per hit, in order>. Voices: breath and
effort only, wordless. No music.

HOLD
<the locks and what holds, once more, in the same words>
```

- The mentions wire each source into `reference` and the run sends them numbered ("Mara Venn (image 1)"); in a
  direct `generate_media` call write "Image 1 is Mara" by hand, in the order of the references.
- **Picks up / Leaves them** on every shot is what keeps the action continuous across the cuts: each shot starts
  exactly where the last one ended.
- Shots of 3–5 s inside a pass. Put cut times on the beat boundaries of the choreography table.
- When a card can't carry a field the mode needs, make the card in one call:
  `generate_media {model, mode, input, canvas: {name, frame}}`.
- Sound on for a pass (impacts in sync are hard to add later); say what each hit sounds like, or they are
  silent.

## 7. Look, then fix

You don't see video. Grab a sheet of frames, two per second, and `read` it against the table beat by beat
(`tile=10x3` covers 15 s, `10x6` covers 30 s):

```sh
f="canvases/ep01-the-bell/assets/<P3A take>.mp4"
ffmpeg -hide_banner -loglevel error -y -i "$f" -vf "fps=2,scale=240:-2,tile=10x3" -frames:v 1 /tmp/p3a.png
```

| Problem | Change |
|---|---|
| Hits stop short or pass through | add the reaction along the strike's direction; both bodies in frame at contact; a sound per hit |
| Floaty, weightless | feet and ground in every beat; the place reacting; the real-speed lock |
| Generic kung fu, spinning, flourishes | remove every technique name; geometry only; stance in the first frame |
| Wind-ups, telegraphed swings | delete preparation words and any "no wind-up"; use the positive lock |
| Drifts into slow motion | "real speed, one speed throughout" in the opening line and in HOLD |
| Later beats ignored or blurred | too many beats: cut some, or split into two passes at a stillness |
| Damage heals, the dropped thing is back | list it with its second under WHAT HOLDS; repeat in HOLD |
| The two fighters swap or merge | distinct silhouettes and colours; name each by outfit at every beat; sheets wired as references |
| Extra people appear | state the count as a lock; check the place reference is empty |
| The cuts land at the wrong times | fewer shots; cut times on whole or half seconds |
| A provider refuses the prompt | rewrite injury as swelling, dust, sweat and torn cloth; drop wound words |

Change one thing per retry and make it a new take, so the takes can be compared.

## 8. Variations

- **Two against one**: never both at once. Move the lead so the opponents line up and one blocks the other;
  deal with the nearer; the second comes around the falling body. A different pattern for each.
- **Weapons**: which hand holds it, which part lands, and where it ends up. A dropped weapon is damage: it stays
  where it fell.
- **A chase**: one direction of travel, three landmarks, one obstacle per shot, the gap between the two stated
  in each shot ("ten paces behind", "an arm's length").
- **A battle or a crowd**: models lose individuals above four people in frame. Show it as a short wide with
  sound doing the work, then stay on the duel inside it.
- **A fall or a stunt alone**: the same rules, with the ground as the second body: where the body lands, on
  what part, what the surface does, how long it lies still.

## Pitfalls

- Shot-by-shot generation of continuous action: use a pass.
- Technique names and prohibitions in the prompt: both bring back what you were avoiding.
- A fight with no stillness: nothing reads as fast.
- Faces and dialogue inside the pass: make them as singles and cut them in.
- Choreography that lives only in a prompt: keep it in the pinned document, where the user can change it.

## Next

The pass note and frames → `shot-listing`. Modes and per-model wording → `video-prompting`. Singles and lines
→ `acting-direction`. Animated shows → `animation-direction`. Cutting passes, covers and inserts together →
`assembly`.
