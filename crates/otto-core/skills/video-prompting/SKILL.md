---
name: video-prompting
description: Writing video generation prompts and choosing the mode (text-to-video, image-to-video from a keyframe, first+last frame, reference-to-video, video edit/extend, motion transfer) with per-model practice for Kling, Seedance, Veo and Gemini Omni, Wan, MiniMax H3, Grok Imagine and others in the catalog. Load before any generate_video or video generate_media call, when a clip came out wrong and needs a retry, or when choosing a video model for a shot.
---

# Video prompting

Model ids, modes and fields change monthly. Before a model's first use in a conversation: `list_media_models
{kind: "video"}` (modes, durations, inputs) and, for anything beyond prompt + frames, `media_model_schema
{model, mode}`. Trust the schema over this file. Keyframe stills are covered by `image-prompting`.

## 1. Choose the mode

| Shot needs | Mode | The video card's wires |
|---|---|---|
| A look you have approved as a still (default) | image-to-video from the keyframe | `first_frame` ← the key card |
| Continuous action across a cut | image-to-video from the previous shot's last frame | `first_frame` ← that frame (`continuity`) |
| A move or change between two known images | first + last frame | `first_frame`, `last_frame` |
| Characters that must hold through big motion, two characters interacting, no keyframe | reference-to-video | `reference` ← the Entity nodes (sheets) |
| Mood, establishing, no recurring entity | text-to-video | none |
| A good clip with one thing wrong | video edit | `video` ← the clip's card |
| A clip that should go on | video extend | `video` ← the clip's card |
| Continuous physical action (a fight, a chase) | a multi-shot pass: reference-to-video, long duration, timed shots (`action-choreography`) | `reference` ← the sheets, the place, a blocking image |
| The same action from new camera angles | re-coverage: reference-to-video with the clip as the motion source | `video` ← the clip's card, `reference` ← the sheets |
| A scene of talk with recorded voices | a part per one to three lines: reference-to-video with images, video sheets and the recordings (`dialogue-scenes`) | `reference` ← sheets and set views, `video` ← video sheets, `audio` ← the Speech cards |
| A performance or move filmed elsewhere, on our character | motion transfer / motion control | `video` ← the source clip, `reference` ← the sheet |

Keyframe first is the default because stills are cheap, reviewable and carry the style; video is not. With the
card's model on Automatic, the run picks a model that takes what is wired; a chosen model that can't take a
wire fails on the card and says which. `middle_frame`, `video` and `audio` need a model with a field for them.

## 2. The card

A video card per shot, in its shot frame, titled `S2.3 video` (`shot-listing` may have laid it out as a draft):

```
canvas_edit {canvas: "ep01-pilot", ops: [{op: "add_node", node: {parent: "n_sh23", body: {kind: "gen",
  makes: "video", title: "S2.3 video", model: "kling-3.0-pro", prompt: "<motion prompt, §3>",
  options: {<duration 5, sound off; names per the tool's description>}}}}]}
then add_edge {from: <S2.3 key card>, to: <the video card>, role: "first_frame"}, and canvas_run it.
```

- Its prompt has no style line and no aspect: the run appends the style line, then the motion line when
  `world/style.md` has one (`animation-direction`), and sizes from the shot's format.
- `canvas_run` runs each card as a background job (one result message per card); video takes minutes. Start
  a scene's video cards in one call and keep working. Run them once their keyframes are approved; a card wired from one that has never run fails
  ("Run S2.3 key first") unless both are in the same `canvas_run`.
- **duration**: the shot's generated length from its note (cut + ~1 s handle), within the model's range.
- **sound**: off when dialogue and SFX are made separately (`sound-design`); on when using a model's native
  dialogue or when the clip's own ambience is wanted.
- For model-specific fields (`multi_prompt`, `elements`, `negative_prompt`, `draft`, `cfg_scale`), read
  `media_model_schema {model, mode}` and set them in the card's `options` with its `mode` (see the tool's
  description); when a card can't carry them, `generate_media {model, mode, input, canvas: {name, frame}}`
  makes the card in one call (media fields take project paths). Reference order matters on models that number
  them (below): on a card, mention each source instead of numbering it (`@characters/mara`, a look or angle
  of it `@characters/mara#hood-down` (a reference's id is its file's name without the extension), `@#<clip card>`). The mention wires an image into `reference` (wire a
  clip or audio into `video`/`audio` yourself) and the run writes it out with its number as sent: "Mara Venn
  (image 1)", "Mara Venn, Hood down (image 2: take only her hair once the hood is down)", "S2.2 video (video 1)",
  "S2.3 key (the first frame)". Number by hand only in a direct call.
- Outside a canvas (a one-off clip, no studio), `generate_video {prompt, first_frame, references, duration,
  sound, aspect_ratio}` with the style line (and the motion line) at the end of the prompt.

## 3. Prompt structure (all models)

Write in this order, as plain sentences, 40–120 words for a single shot. (A multi-shot pass is a long
sectioned prompt with its own layout: `action-choreography` §6. A shot whose point is a face adds a
performance block after the light: `acting-direction` §2.)

1. **Shot**: size, angle, lens feel (`Medium close-up, eye level, 50mm, shallow depth of field.`)
2. **Subject**: who, by the appearance words from their entity file (only for text- and reference-to-video).
3. **Action**: one main action, with its start and end state and its pace (`slowly lifts her head and looks
   toward screen-right`).
4. **Camera**: one move, with direction, amplitude and speed (`The camera pushes in slowly, small amplitude.`).
5. **Setting and light**: place, time, weather, light direction (from the continuity note).
6. **Style**: nothing on a card (the run appends the style line); in a direct call, the style line, unchanged.
7. **Sound**: what is heard, or what is not (`No dialogue. No music. Ambient: gulls, water on the hull.`).

**Image-to-video: describe the motion, not the picture.** The frame already fixes subject, place, wardrobe
and style; repeating them invites the model to redraw them. Say what moves, how, in which direction, how fast,
what the camera does and what is heard. Name things only to anchor motion ("the woman", "the rope").

**Camera words** every current model understands: static / locked-off; push in / pull out; zoom in / out
(lens, camera stays); pan left/right; tilt up/down; truck left/right; pedestal / crane up/down; tracking
or follow shot; arc / orbit (with degrees); handheld; POV; dolly zoom; aerial / drone; rack focus (say what
goes in and out of focus). Add amplitude (`small`, `large`) and speed (`slow`, `fast`) only when they matter.

**Leave out**: praise words (`masterpiece`, `beautiful`, `4K`), contradictory moves (static + pan), several
moves at once, counts of fast repetitions ("blinks 3 times"), long lists of objects, text you don't need
rendered, and anything the keyframe already shows.

**Every reference gets one job, and its limits.** Say what each wired image or clip is for and what stays
behind in it, or the model takes everything: a sheet's white background, a plate's empty street, a source
clip's camera. `Image 1 is Mara: her face, build and outfit; its white background and flat light stay behind.
Video 1 is the action only, second for second; its camera angles, cuts and sound stay behind.`
- A character's or prop's **video sheet** (`character-design` §6): "her face, build, outfit, the way she moves
  and her voice; ignore its studio, its orbit and what she does in it".
- A reference wired for consistency that is not in this shot: `context only; it does not appear in frame`.
- Where the scene differs from the reference, an `OVERRIDE:` in capitals stating what is there instead ("her
  hood is DOWN in every shot; her whole head is visible"). A plain mention loses to the picture.
- A place lit differently from the scene: "its geometry and materials only; ignore its light and exposure". On a card the
same lines start `@characters/mara is Mara:` and `@#n_p3a is the action only:`; a named reference's
`defines` is its job already (the run says it), so mention it rather than restating it.

**Negatives**: for things, people and actions, state what is there instead of what isn't. Naming something in
a "no …" tends to bring it into the picture ("no wind-up" makes a wind-up, "no extra people" adds a passer-by):
write `two people in the room, both in frame`, `one continuous shot, one camera`, `real speed throughout`. Plain
`No subtitles. No text. No music.` at the end do work, since those are overlays and audio the model adds on
top. In a long sectioned prompt, close with **counts**: "People in frame: one. Tears: zero. Button presses:
exactly one. Exactly four shots, hard cuts at 3.0, 6.0 and 11.0 s, 20 seconds in all." A number is a state
the model holds, and the last lines of a prompt are the ones it keeps. When the schema has a `negative_prompt` field, put a short list there instead of in the prompt.

## 4. Check, keep, retry

When the result arrives, look at it: `read` the poster or grab frames (`continuity` §5 has the `ffmpeg`
line). Check faces against the sheet, hands, count of people, wardrobe, text, the first and last second.

| Failure | Retry with |
|---|---|
| Face or outfit drifts | add the sheet (and turnaround) as references; shorter clip; less camera motion |
| Morphing, melting, extra limbs | simpler action, slower; one subject; shorter duration; describe the end state |
| Ignores the camera move | put the move first in the prompt; one move only; on Kling raise `cfg_scale` a little |
| Unwanted cuts in one clip | `One continuous shot, no cuts.` first (Gemini Omni, Wan, Seedance, Kling multi-shot off) |
| Garbled text or captions | `No subtitles, no text`; put needed text in the keyframe, not the prompt |
| Static clip ("poster") | the prompt describes the image, not the motion; name the first action |
| Unwanted speech or music | `No dialogue. No music.`, or `sound: false` |
| The thing you ruled out appears | a "no …" named it: replace it with the positive statement |
| A reference's background, light or camera leaks in | give that reference its one job and say what stays behind |
| A dead, glassy or over-acted face | `acting-direction`: a task for the eyes, no feeling adverbs, a keyframe at rest |
| Lips off the words, a frozen listener, a voice that changes between shots | `dialogue-scenes`: record first, time the beats to the recording, short parts |
| Hits that miss, weightless action, generic fighting | `action-choreography`: reactions, geometry, positive locks, a pass |
| A stylised look turning glossy, smooth or photoreal | `animation-direction`: keyframe first, the motion line, held poses |

A retry is a re-run of the card with the better prompt or wires: a new take, picked. Make 2 takes of hard shots
rather than 5 retries of the same prompt, and pick the better one (`update_node` its `pick`; not on a card where
the user already chose). The picked take is the clip `assembly` uses. When it needs trimming (a bad first second,
a handle), add `- Cut: in 0.4 out 4.6 (take <file name>)` to the shot note. When the best of a shot is spread
over takes (a performance, a pass and its re-coverage), write several `Cut:` lines in order, each naming its
take: the shot is those passages joined (`shot-listing` §3).

## 5. Per model family

### Kling (3.0, 4.0, O3/Omni, 2.x motion control)
- Kling's own guide: prompts read as directed scenes; 3.0 makes 3–15 s, native audio in several languages,
  dialogue per character, and multi-shot.
- **Multi-shot**: `multi_shots: true` lets the model cut; `multi_prompt` (up to 6 items, each `{prompt ≤512
  chars, duration}`) scripts the shots. Keep it off for a single shot of the shot list.
- **Dialogue** (native audio): name the speaker, tone in brackets, line after a colon:
  `Mara (quietly, tense): "Not again."` Kling assigns lines to characters this way; accents and languages
  can be named (`in Spanish`, `with a Scottish accent`).
- **Elements** keep a subject fixed: on fal, `elements` take `frontal_image_url` + 1–3 `reference_image_urls`
  (the sheet and turnaround) and are named `@Element1` in the prompt; reference images are `@Image1`. Check
  the schema of the endpoint you use: Higgsfield's Kling 3.0 Pro takes `elements` as a list of strings.
- `cfg_scale` (0–1, default 0.5) raises prompt adherence; `negative_prompt` exists on some endpoints.
- Kling 2.6/3.0 motion control (`mode: "motion-control"`, `image_url` + `video_url`) animates a still with the
  motion of a reference video.

### Seedance (2.0, 2.5)
- ByteDance's guide: `Subject + Location + Event + Style + Camera`, then what must stay consistent.
- References are numbered by type in upload order: `Image 1`, `Video 1`, `Audio 1`. Bind each in the
  prompt and say what it is for: `Mara is Image 1; the harbor is Image 2; follow the camera move of Video 1.`
  Don't rely on names written inside images. When a reference is precise, say "strictly refer to" it instead
  of re-describing it.
- Timelines: `0-3s: … 3-7s: …` or `Shot 1: [Close-up, static] …`, contiguous, one-second units; don't pack
  too much into a span (it adds cuts or drops beats).
- Prefer positive description; negatives are reliable for subtitles and audio (`No subtitles.`, `No BGM;
  ambient and action sounds only.`).
- First/last frame through `image_url`/`end_image_url` locks the output aspect to the image. 2.5 has
  `video-edit` (`Only change …; keep everything else unchanged.`, timestamps allowed) and `video-extend`
  (`Continue Video 1: …`), and `draft: true` for a 480p draft finalised later.
- 2.5 makes up to 30 s and takes many reference images and several reference videos (read the schema for the
  counts), which is what a multi-shot pass and its re-coverage need: timed shots with hard cuts in one
  generation, then the same action re-shot from new angles with the first clip as `Video 1`. A 480p draft is
  a cheap way to check a long pass's action before the full run.

### Veo 3.1 and Gemini Omni Flash (Google)
- Veo formula: `[Cinematography] + [Subject] + [Action] + [Context] + [Style & ambiance]`. Clips are 4, 6
  or 8 s. Dialogue in quotes (`She says, "We leave now."`), `SFX: …`, `Ambient noise: …`. Express negatives
  as absences ("a desolate street with no cars"); fal's Veo endpoints also take `negative_prompt`. The
  `first_frame` should be 720p+ in 16:9 or 9:16, or it is cropped.
- Gemini Omni Flash makes several shots by default: write `In a single continuous shot, no scene cuts.` for
  one. Durations 3–15 s; first+last frame 3–10 s; up to 6 reference images (at most 3 per character, 2
  characters). Timing in plain words or `[0-5s] …`. Describe the audio you want, including music. Don't
  write control tags like `<FIRST_FRAME>`; say in words how each image is used.
- Omni edits: short prompts work best, ending in `Keep everything else the same.` Extensions continue
  from the last 10 s of the clip; re-mention anything from earlier.

### Wan (3.0, Prime)
- Alibaba's formula: overall description → reference citations (`Image 1`, `Video 1`, `Audio 1`, numbered
  per type) → `Shot 1 [0-3s]: …` → dialogue `Mara says: "…"` → SFX/BGM → style and mood → a negative list
  at the end (`No subtitles, no watermark, no face distortion.`). Skip sections you don't need.
- First line `One continuous shot.` for no cuts; `Fixed shot, camera static.` for a locked camera. Shots
  2–5 s each, contiguous. `No dialogue.` / `No background music.` when wanted, otherwise it may add them.
  `Lip sync` after a line asks for synced mouths; `Voice timbre references Audio 1` clones a voice.
- Image-to-video takes `image_url` + optional `end_image_url`; reference-to-video takes up to 10 images, 5
  videos, 5 audio clips. Small on-screen text is weak: keep it in the keyframe.

### MiniMax H3
- MiniMax's own prompt skill uses three labelled fields: `integrated_multimodal_description:` (visuals,
  action, camera, speakers, timeline; later shots as `[Shot 2] At 00:03.500, the camera cuts to …`),
  `overall_soundscape:` (ambience, foley), `non_diegetic_music:` (score, or `None.`). Fill both audio fields,
  even with `None.`; left empty they produce unpredictable sound.
- Camera = type + amplitude + speed as a sentence (`The camera pushes in with small amplitude at slow speed
  toward her hands.`). One coherent plan per clip. For i2v, describe the first action that leaves the frame.
- First/last frame and references are separate modes; don't mix them in one request.

### Grok Imagine (xAI)
- Image-to-video: describe what moves and how, with direction (`slow push-in`, `pan left`, `embers drift
  upward`); short prompts let it improvise, detailed ones give control. 1–15 s.
- Reference-to-video: up to 14 images (720p max). On xAI's API they are `<IMAGE_0>`, `<IMAGE_1>`… in list
  order (a pinned first frame takes `<IMAGE_0>`). Give each reference one role, keep the cast small and props
  static. Voices are preset `voice_id`s (`<AUDIO_0>`). Pinned frames: first, last and up to 4 interior
  keyframes; check `media_model_schema` for which of these the endpoint here exposes.

### Others in the catalog
- **Motion transfer / object swap / restyle** (e.g. `genjutsu`): `video_url` + `image_urls` (1–8: the sheet,
  turnaround, place); prompt says what to keep (motion, camera) and what to swap.
- **LTX, PixVerse, FLUX video, Happy Horse, Cinema Studio**: read the schema; use the general structure.
- **Lip sync** (`kling-lipsync-audio-to-video`, `lip-sync/*` modes): see `sound-design`.

## Next

Chaining shots and checking consistency → `continuity`. Fights, chases, stunts → `action-choreography`.
Faces and performance → `acting-direction`. Scenes of talk → `dialogue-scenes`. Animated and stylised motion →
`animation-direction`. Dialogue, SFX, music → `sound-design`. The cut → `assembly`.
