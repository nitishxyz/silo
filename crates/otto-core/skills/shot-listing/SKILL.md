---
name: shot-listing
description: Turning an episode's script.md into scene and shot frames on its canvas, with one note per shot (framing, angle, movement, action, entities, duration, dialogue and sound, generation plan), the scene's Entity nodes, and the shot's cards and wires laid out ready to run. Load after series-writing has produced a script and before any keyframe or video is made, when the user asks to "break down", "storyboard" or "shot list" a scene or episode, or when shots need re-planning for another format (vertical vs landscape).
---

# Shot listing

The shot list is the production plan, and on the canvas it is also the graph that makes the episode: each shot
frame holds its note and its cards. `image-prompting` writes and runs the keyframe cards, `video-prompting` the
video cards, `continuity` chains them, `sound-design` adds voice and sound cards, `assembly` cuts the picked
takes in frame order. Keep it on the canvas, not in the chat.

## 1. Read before planning

1. `studio_read`: formats, the canvas's format, entities and their ids.
2. `read canvases/<ep>/script.md` and `world/style.md`; `read` the entity files of everyone and everywhere in it.
3. `canvas_read {canvas}`: if frames already exist, update them instead of adding duplicates.
4. `list_media_models {kind: "video"}` (and the user's `[models] video` in `studio.toml`): note the durations the
   chosen model can make (e.g. 3–15 s, 4/6/8 s only, 2–30 s). Shot lengths must fit them.
5. Work out the effective format (shot frame, then canvas, then project). Composition rules depend on it (§5).

## 2. Break the script down

For each scene heading in the script, one scene frame. Within it, shots in story order. Per scene decide:

- **Coverage**: open on where we are (an establishing wide or a detail that places us), then the action at
  the shot size that serves it. Dialogue: a two-shot or over-the-shoulder to set geography, then singles
  (MCU/CU) on each speaker, reaction shots, back wide when someone moves. Inserts for props that matter.
- **Pacing**: shots of 2–4 s for tension, action and vertical shorts; 4–8 s for mood, landscape, slow
  dialogue. A 60 s vertical episode is roughly 15–25 shots; a 3 min landscape episode 35–60.
- **What a model can do in one clip**: one subject action and one camera move. "She turns, walks to the
  door, opens it and looks back" is two or three shots. Split until each shot is one clear beat.
- **Continuous physical action is the exception**: a fight, a chase, a fall. Cut from one-beat clips it
  stutters, because each clip guesses pose and tempo again. Plan it as a **pass**: one generation holding
  several timed shots with hard cuts, on a model that makes long clips (§3, `action-choreography`).
- **Faces carry dialogue scenes**: give each shot whose point is a face an `Acting:` line, and enough length
  for the silence around the line (`acting-direction`). A scene of talk is planned in **parts**, each a pass
  of one to three lines timed to recordings made first (`dialogue-scenes`): plan the parts, not single shots.
- **Assets before shots**: every recurring character, prop and place a scene shows has its approved sheet (and
  for video scenes its video sheet and set views) before the scene's cards are built (`character-design`,
  `prop-design`, `location-design`). Check them in `studio_read`'s Production block: name what has no sheet
  and offer to make it, and what isn't approved (or changed since) and ask the user to approve it.
- **Cuts carry story**: plan what changes at each cut (new information, a reaction, a new angle). Never two
  shots of the same size and angle on the same subject back to back (a jump cut) unless that is the style.

Shot vocabulary (use these words in notes; `video-prompting` maps them to each model):

| Size | | Angle | | Movement |
|---|---|---|---|---|
| EWS, WS | extreme wide, wide | eye level | | static / locked-off |
| MWS / cowboy | knees up | low angle | heroic, threat | push in / pull out (dolly) |
| MS | waist up | high angle | small, watched | truck left/right, track / follow |
| MCU | chest up | overhead / top shot | | pan, tilt |
| CU, ECU | face, a detail | Dutch | unease | arc / orbit, crane up/down |
| OTS, POV, two-shot, insert | | | | handheld, slight shake |

## 3. The shot note

One Note node per shot, inside its shot frame. Fixed format so other skills can parse it. The note plans the
shot; it is not a prompt (don't wire it into a card's `text` port):

```markdown
**S2.3 · Mara hears the bell** · 4 s (gen 5 s)
- Frame: MCU, eye level, 3/4 front, Mara left third looking screen-right
- Camera: slow push in
- Who: characters/mara#oilskin (hair wet)  ·  Where: places/harbor#reverse (dawn fog, low sun from screen-left)
- Props: props/old-bell (off-screen)
- Action: She stops coiling rope, lifts her head toward the sound; breath fogs.
- Dialogue: MARA (on-screen, under breath): "Not again."
- Sound: distant bell toll on the cut; gulls; water against hull
- Plan: keyframe → image-to-video · kling-3.0-pro · sound off (dialogue separate)
- Chain: first_frame ← last frame of S2.2
```

Lines added later, only when there is something to say:
- `- Acting: keeps working, checks sideways whether Teo believed her`: the character's tactic in one line, from
  the scene's Performance note (`acting-direction`).
- `- Cut: in 0.4 out 4.6 (take <file name>)`: trim points on the picked take of the shot's video card
  (`video-prompting`). One `Cut:` line: the clip used is that picked take, and the file name says which take the
  trims fit. Several `Cut:` lines: the shot is those passages joined in order, each from the take it names (any
  take of a video card in this frame), for a performance picked from several takes or a pass cut together
  with its re-coverage.
- `- Audio: S2.3 mara-01 @0.6 dlg · S2.3 sfx-rope @1.2 sfx`: the voice and sound cards `assembly` mixes under
  this shot, by card title, with the offset from the shot's start and a role (`sound-design`).

**A pass** is one shot frame too: one note, one generation, several shots inside it. Number it `P<scene><letter>`
and keep the same field format:

```markdown
**P3A · The quay fight** · 15 s (gen 15 s) · pass, 4 shots
- Who: characters/mara, characters/warden-oske  ·  Where: places/old-bell-quay (dawn fog)
- Choreography: canvases/ep01-the-bell/fight-sc3.md (pinned in the scene frame)
- Shots: 1 0.0–3.5 WS profile · 2 3.5–7.0 MS over Mara · 3 7.0–11.5 low WS · 4 11.5–15.0 MCU Mara
- Starts: Mara at the winch, Oske on the top step  ·  Ends: Oske down by the bollard, lamp broken, compass gone
- Sound: clip sound on (impacts, breath); no music in the clip
- Plan: pass · reference-to-video · seedance-2.5
```

`Starts` and `Ends` are the states the shots before and after it must match. The beats themselves live in the
choreography document, not in the note.

Field rules:
- **Ids, not names**, for `Who`, `Where`, `Props`: entity ids from `studio_read`, plus the state that differs
  from the sheet (wet hair, torn sleeve). When the shot needs a look or an angle that isn't the sheet, name the
  entity's named reference by its id (its file's name without the extension, as `studio_read` lists it):
  `characters/dhritarashtra#crown-off`, `places/harbor#reverse`. These
  pick the references later: the card's prompt mentions exactly them. A state with no reference yet is a
  reference to make first (`character-design` §4). `Props` takes the one the character's `links` name.
- **Duration**: the cut length, then the generated length in brackets when the model's minimum or a
  trim handle makes it longer (generate about 1 s more than the cut and trim in `assembly`).
- **Plan**, one of:
  - `keyframe → image-to-video`: the default. Cheap still first, approved, then animated.
  - `reference-to-video`: when the shot needs entities the keyframe can't hold steady through big motion, or
    two characters interacting; pass sheets as references (`video-prompting`).
  - `first+last frame`: a move or transformation between two known images (a reveal, a time-lapse).
  - `chain`: `first_frame` from the previous shot's last frame, for continuous action across a cut.
  - `pass`: several timed shots in one generation, for continuous physical action (`action-choreography`).
  - `still`: a held keyframe with a slow push in `assembly` (titles, cutaways, budget), or held for a few
    frames as an impact insert (`animation-direction`).
- **Dialogue** says on-screen or off-screen (`O.S.`, `V.O.`); `sound-design` decides lip-sync from it.

## 4. Put it on the canvas

Create the frames, then the notes, with `canvas_edit`. Nodes added without `x`/`y` are placed after the last
node in their frame; use `layout` to tidy. One call per scene keeps ids manageable:

```json
canvas_edit {"canvas": "ep01-pilot", "ops": [
  {"op": "add_node", "node": {"body": {"kind": "frame", "title": "Harbor at dawn", "role": "scene", "order": 2}}}
]}
```
The result returns the scene frame's id (say `n_sc2`). Then its shots and notes:

```json
canvas_edit {"canvas": "ep01-pilot", "ops": [
  {"op": "add_node", "node": {"parent": "n_sc2", "body": {"kind": "frame", "title": "Mara hears the bell", "role": "shot", "order": 3}}},
  {"op": "add_node", "node": {"parent": "n_sc2", "body": {"kind": "frame", "title": "The bell tower", "role": "shot", "order": 4}}}
]}
```
Then one `add_node` per note with `parent` set to its shot frame id and `body: {"kind": "note", "text": "…"}`,
and `{"op": "layout", "ids": [shot ids…], "as": "row", "in": "n_sc2"}` so shots read left to right.

The script itself stays on the canvas as its document card (pinned by `series-writing`; pin it top-left with an
`add_node` of an asset whose `path` is `canvases/<ep>/script.md` if `canvas_read` lists it as not on the
canvas): shot notes plan each shot, they don't copy the script. Wire the script into a card's `text` port only when
its text should be context for that generation (it adds the whole script, up to 8,000 characters, to the prompt).

Also per scene: one Note in the scene frame titled `**Continuity · Scene 2**` (the `continuity` skill fills it),
and Entity nodes (`{"kind": "entity", "id": "characters/mara"}`) for the scene's cast, place and props, outside
the shot frames, so `reference` wires have something to come from.

**The shot's cards.** Each shot frame gets, beside its note, the cards its `Plan` names, titled from the shot
number so every skill finds them:

| Card | Title | Wires in |
|---|---|---|
| Image (the keyframe) | `S2.3 key` | `reference` ← the Entity nodes of `Who`/`Where`/`Props` (characters first), or an asset node of the named reference the note gives |
| Video | `S2.3 video` | `first_frame` ← the key card (or the chained last frame); `reference` ← sheets for reference-to-video |
| Speech / Sound | `S2.3 mara-01`, `S2.4 sfx-bell` | made by `sound-design` |
| Video (a pass) | `P3A video`, then `P3A cover`, `P3A finish` | `reference` ← the Entity nodes and the blocking image; `video` ← the pass card before it (`action-choreography`) |

Add them as drafts once the frames exist (`add_node` with a `gen` body: `makes`, `title`, `prompt`, and `model`
when the plan names one), then the wires with the ids the result returned (`add_edge` with `from`, `to` and the
port as `role`). A draft prompt that already names its cast as mentions (`@characters/mara at @places/harbor`)
wires their sheets by itself, Entity nodes added where missing; a note's reference is mentioned the same way
(`@characters/mara#oilskin at @places/harbor#reverse`) and wires that file. Prompts can wait for `image-prompting` and `video-prompting`; a draft card with the plan
already wired shows the user the whole episode before anything renders. A `still` shot has only a key card.

- `order` numbers scenes across the canvas and shots within their scene, from 1. Renumber with
  `update_node {id, body: {"order": n}}` when inserting; don't leave gaps or duplicates.
- A shot that needs another format than the canvas (a vertical insert for a social cut) gets
  `"format": "vertical"` on its frame.
- Number shots `S<scene>.<shot>` in note titles and card titles; files you make yourself are named from it
  (`assets/frames/s02-03-last.png`).

## 5. Composition per format

**16:9 landscape.** Rule of thirds, lead room in the look direction, room for two-shots and OTS. Keep titles
and burned captions inside the centre 90% (title-safe), action inside 93%.

**9:16 vertical.** One subject per frame, centred horizontally, eyes at about 35–40% from the top. Prefer
MCU/CU and MS; wides read as small, so use them briefly and with a strong silhouette. Stack depth vertically
(foreground bottom, subject middle, sky top) rather than side by side. Two people: stagger them in depth or
cut between singles; don't try to fit a side-by-side two-shot. Platform UI covers roughly the top 13%, the
bottom 20% and the right 12% (buttons, captions, handle); keep faces, props that matter and burned text out of
those bands. Captions sit just above the bottom band. Hook in the first 1–2 s: open on the most striking
image, not an establishing shot.

**1:1 and 4:5.** Centred, tighter than landscape; MS and MCU; keep the top and bottom 8% clear for feed UI on 4:5.

**One episode, two formats.** Plan for the strictest format if both are wanted: keep the subject inside the
centre 9:16 region of a 16:9 keyframe, so `assembly` can crop a vertical cut. When framing must really differ,
make a second canvas with `format: "vertical"` whose key cards take the first canvas's keyframe takes as
`reference` (asset nodes of their files) and re-frame them.

## 6. Worked example

Script excerpt (`canvases/ep01-pilot/script.md`, vertical, Kling 3.0 Pro: 3–15 s):

```
SCENE 2 — EXT. HARBOR — DAWN
Fog. MARA coils rope on her boat. A bell tolls somewhere it shouldn't. She freezes.
MARA: Not again.
She looks to the old bell tower across the water. TEO steps out of the cabin behind her.
TEO: You heard it too.
```

Shots:
1. **S2.1** · 3 s (gen 4) · WS, high angle, static: the boat small in fog, Mara a silhouette at bottom third.
   Plan: keyframe → i2v. Sound: water, gulls, fog ambience.
2. **S2.2** · 3 s · CU insert, overhead: wet hands coiling rope. Plan: keyframe → i2v.
3. **S2.3** · 4 s (gen 5) · MCU, eye level, slow push in: Mara lifts her head. Dialogue on-screen:
   "Not again." Plan: keyframe → i2v; lip-sync handled per `sound-design`. Chain: none (new angle).
4. **S2.4** · 3 s · POV from Mara, static, slight handheld: bell tower across the water through fog. Plan:
   keyframe from `places/bell-tower` sheet → i2v. Sound: second toll, closer.
5. **S2.5** · 4 s (gen 5) · MS, Mara foreground lower frame looking screen-right, Teo behind in the cabin door,
   rack focus to Teo: "You heard it too." Plan: reference-to-video with both sheets (two characters
   interacting).

Scene length 17 s; five shots; every beat of the scene has a shot; S2.4 is the eyeline match for S2.3.

## Pitfalls

- Shots with two actions or two camera moves: models do one well. Split them.
- A fight or chase split into one-beat clips: plan it as a pass.
- Durations the model can't make: 2 s on a 5 s-minimum model means generating 5 s and trimming; plan it.
- Forgetting the cutaway: every dialogue scene needs one or two shots that don't show a mouth (`sound-design`).
- Wides in vertical: faces become specks. Use them as 1–2 s establishing beats only.
- Notes in prose: keep the field format; other skills parse it.
- Cards outside their shot frame, or titled freely: `assembly` and `continuity` find a shot's cards by frame and
  title.

## Next

Keyframes per shot → `image-prompting` (with `continuity` for references and chaining). Video →
`video-prompting`. Fights, chases and stunts → `action-choreography`. Performance in dialogue shots →
`acting-direction`. Scenes of talk → `dialogue-scenes`. Motion in an animated show → `animation-direction`.
Lines, SFX, music → `sound-design`. The cut → `assembly`.
