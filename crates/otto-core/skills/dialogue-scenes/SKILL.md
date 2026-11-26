---
name: dialogue-scenes
description: Making a scene of talk in a Studio hold together - a conversation, a phone or video call, an argument, an interview - by recording every line first and timing the picture to the recordings, splitting the scene into short parts that are each one multi-shot generation, an input map that gives every image, clip and recording one job, shots written as a first frame plus beats timed to the words, a listener who stays alive, one screen direction, counted constraints, continuing from the previous part, and cutting the scene from passages of several generations. Load for any scene with two or more spoken lines before its video cards, and when faces drift, lips miss the words, a character freezes while the other speaks, or voices change between shots.
---

# Dialogue scenes

Talk is where generated video falls apart: faces drift, lips miss the words, the listener freezes into a
photograph, the voice changes from shot to shot. Three decisions prevent most of it:

1. **The voices exist before the picture.** Each line is recorded first; the video is timed to the recordings
   and lip-synced to them, instead of the model inventing a voice and a rhythm.
2. **The scene is made in short parts.** One generation carrying a whole conversation lets the words and the
   acting slip. A part holds one to three lines.
3. **Every reference has one job**, written down, with what to take and what to leave.

This skill is the method. What each person is doing to the other is `acting-direction` (do that first: the
scene's Performance note). Voices, tags and the mix are `sound-design`; modes and per-model wording are
`video-prompting`; an animated show adds `animation-direction`.

## 1. Before the scene: its assets

A dialogue scene reuses the same few things in every shot, so they are made, fixed and approved first:

- each speaker's sheet, and for anyone seen from several angles their **video sheet** (`character-design` §6);
- the set, with the views the coverage needs (`location-design` §3: several views that agree) and any vehicle or
  machine it happens in (`prop-design`);
- what the scene shows that exists nowhere else: a screen's contents, a photo on the wall, the other end of a
  call. Each is an image card in the scene frame, made once and wired into every part. A small prop's position
  gets a placement image (`prop-design` §6).

Check a video model that takes reference images, reference videos **and reference audio** is connected
(`list_media_models {kind: "video"}`, then `media_model_schema {model, mode}` for the counts). Without audio
references, record the lines anyway and follow `sound-design` §2 for lip sync or cutaways.

## 2. Record the lines

One Speech card per line, in the scene frame, in each speaker's cast voice (`sound-design` §3 has the call and
the tag practice). For this method:

- **Direct the delivery in the text**, at the point where it changes:
  `[small nervous breath] Hello? [hurried, avoiding] I only have the last sector left, [gets cut]`. The breath,
  the sigh and the hesitation that the scene needs are *in the recording*; the picture then acts to them.
- **An interrupted line ends where it is cut** (a dash), and the interrupting line is its own card.
- Run each card twice (the lines the scene turns on, three times) and pick by the Performance note. The user
  hears them; you don't: say which take you'd pick from its length and ask them to listen to the close ones.
- **Measure every picked take**: its length, and where its pauses fall.
  ```sh
  f="canvases/ep01-the-bell/assets/<take>.mp3"
  ffprobe -v error -show_entries format=duration -of csv=p=0 "$f"
  ffmpeg -hide_banner -i "$f" -af silencedetect=noise=-35dB:d=0.15 -f null - 2>&1 | grep -o 'silence_.*'
  ```
  The silences split the line into phrases ("I checked." 0.0–0.75, pause to 1.3, "There's a good clinic in
  Germany." 1.3–2.9). Those times are what the shot's beats are written to.

Write the result into a `**Lines · Scene N**` note in the scene frame: one row per line with its card, length
and phrase times. Every part is planned from it.

## 3. Split into parts

A **part** is a pass (`shot-listing` §3): one shot frame, one generation, one to four timed shots with hard
cuts, 4–20 s. Title the frames `P2A · The call opens`, `P2B · The clinic line`.

- **One to three lines per part**, cut where the scene changes hands: a new speaker takes over, a silence lands,
  someone looks away. A single line that matters can be a part by itself, one close-up, four seconds.
- **Lay the lines on the part's clock** from the Lines note: the first starts after 0.3–0.5 s of picture, the
  next 0.3–0.8 s after the one before ends (a real silence gets its real length; an interruption *overlaps* the
  end of the line it cuts). The part's length is the last line's end plus a 0.5–1 s hold on a face.
- **Cuts go on line boundaries or inside pauses**, never through a word.
- Give each part's note the pass fields, with `Lines:` listing which recording starts when.

## 4. Wire the part

The part's video card, on reference-to-video (or the model's mode that takes all three kinds):

| Port | From |
|---|---|
| `reference` | each character's Entity node (mention `@characters/emily`), the set views, the scene's own images, the placement image |
| `video` | an asset node of each speaker's video sheet; when continuing, the previous part's clip (§7) |
| `audio` | the part's Speech cards, wired in the order the lines are spoken |

Mention every wired source in the prompt (`@#<node>`): the run writes each out with its number as sent, so the
prompt and the request cannot disagree. Sound on. Style and motion lines are appended by the run.

## 5. The part's prompt

Long and sectioned; the length is positions, times and counts. Write the sections in this order:

```
<Two or three sentences: where we are, who is there, what happens in this part and why it matters. How many
shots. "The spoken lines come from the attached recordings.">

REFERENCES
@#<Mara's video sheet> is Mara: her face, skin, hair, build and outfit, the way she moves and her voice.
OVERRIDE: in this scene her hood is DOWN in every shot; her whole head and hair are visible. Ignore the
reference's studio, its orbit and what she does in it.
@characters/teo is Teo: his face and design exactly. Ignore its white background and standing poses.
@#<the quay, reverse view> is the place: its geometry and materials only. Ignore its daylight: here it is
night, lit only by the lamp.
@#<the letter image> is the letter in close-up: context only in this part; it does not appear in frame.
@#<P2A mara-01> is Mara's voice and line in shot 1. @#<P2A teo-01> is Teo's voice and line in shot 2.

WHO THEY ARE
Mara: <two lines from the Performance note: her state, what she is hiding, what she does to him, how it shows>.
Teo: <the same>. While the other speaks, each stays alive: natural blinks every two to three seconds, small
shifts of the eyes over the speaker's face, the head settling, a breath.

WHERE EVERYTHING IS
<The set by its side names; who sits or stands where, how far apart; what is at whose hand; what is behind
each.> Screen direction for the whole part: every camera stays on the harbour side of the line between them.
Mara looks screen-right, Teo looks screen-left.

SHOT 1 - 0.0–4.0 s - Close-up, Mara.
First frame: <where she is in the frame, which way she faces, what lights her, what is behind her; her
expression at rest>.
0.0–0.4 s: she swallows; her eyes drop.
0.4–3.3 s: lip-synced to @#<P2A mara-01>, which starts at 0.4 s. 0.4–1.15 s, still looking down, quietly:
"I checked." 1.15–1.7 s: a pause, eyes low, lips pressed. 1.7–3.3 s, gently: "There's a good clinic in
Germany." Her eyes lift to him on "Germany".
3.3–4.0 s: she holds the look, waiting.
Camera: at her front-right, eye level, 0.7 m away, a normal field of view, locked.
Composition: her face on the left two thirds, looking into open space on the right where he is; the brightest
point is her eyes; the darkest is the water behind her.

SHOT 2 - 4.0–9.0 s - Over her shoulder onto Teo. <the same four parts>

LIGHT
<Every source that exists in the scene, and nothing else. What is darkest in every frame and what is
brightest: the face that carries the beat. Eyes keep a catchlight.>

AUDIO
No music. Dialogue exactly these lines, each spoken once, one speaker at a time, using the attached recordings
as the voices: lip-sync each speaker to their recording; do not re-voice, re-time, translate or alter them.
Shot 1, Mara - @#<P2A mara-01>, starting at 0.4 s: "I checked. There's a good clinic in Germany."
Shot 2, Teo - @#<P2A teo-01>, starting at 4.3 s: "…"
No other speech. Sounds: <ambience; each effect, in order>, all under the voices. No breaths or sighs beyond
those in the recordings.

COUNTS
People in frame: two. Smiles: zero. Tears: zero; eyes may shine. Times Teo looks away while speaking: exactly
one, on "tomorrow". Readable text: zero. Exactly two shots, one hard cut at 4.0 s, 9 seconds in all. Mara looks
screen-right and Teo screen-left in every shot.
```

What makes it work:

- **Each reference: what it is, what to take, what to ignore.** Three special cases: `context only` for a
  reference wired for consistency that is not in this part's frame; `OVERRIDE` in capitals where the scene
  differs from the reference (no helmet, another outfit), stated as what is there instead; and a reference whose
  light is wrong for the scene gives "geometry and materials only".
- **A first frame for every shot.** Who is where in the frame, facing which way, lit by what. It is the model's
  starting picture after each cut, and where screen direction is kept.
- **Beats timed to the recording**, to a tenth of a second, with the words quoted exactly as recorded. What the
  face does is tied to a word ("her eyes lift on 'Germany'"), and silences have a length and a content.
- **The listener has business.** Blinks, eye shifts, one small action in a silence (brushes a strand of hair
  back, shifts their grip). State it once under WHO THEY ARE and again as a beat where it matters. A listener
  nobody wrote freezes.
- **A thing that must not move is said to be still**: "the contact photo is a frozen still picture; nothing
  inside it moves; only its frame pulses with the ringtone".
- **Actions with hands** are staged for the camera: the hand rises into frame, hesitates above the button, the
  fingertip makes clear contact, the button answers. Nothing between the lens and the contact.
- **Camera per shot in numbers**: where it stands relative to the person, how far, the field of view, and
  whether it is locked or moves and by how much ("a slow 0.15 m push-in").
- **Composition in one or two lines**: what the shot says, where the big, medium and small shapes sit, the one
  lit area, and the colour balance when the scene has a colour idea (most of the frame one colour, a second
  supporting, a small accent). It keeps the face the brightest thing in a dark scene.
- **Light only from sources that exist in the scene.** Name them; say what stays black.
- **Counts instead of prohibitions.** "Count of tears: zero", "exactly one button press", "people physically in
  the room: one". A number is a state the model can hold; "no tears" names tears. Include the shot count, the
  cut times and the total length. Counts go last: they are the final thing read.
- **Breath is in the recording.** Added breathing animation and breath sounds fight the lip sync: rule them out
  by count when a take shows heaving shoulders.

## 6. Check a take

You don't see or hear video. For each take grab a strip at 4 frames per second and `read` it against the beats
(`tile=8xN`; `continuity` §5 has the base command):

- lips closed in the silences and moving on the lines' start times;
- the listener changing from frame to frame (not one repeated picture);
- screen direction, the counts, the override (the helmet that should be gone);
- the right person speaking each line: mouths show it.

Then tell the user which takes hold and ask them to listen for the voice. A model sometimes stretches a pause
or hurries a phrase; that can sound more natural than the plan. Keep it when the user likes it.

| The take shows | Change |
|---|---|
| Lips off the words | one line fewer in the part; start times and phrase times from the measured recording; "do not re-time" |
| The wrong person speaks, or both | "one speaker at a time"; name the speaker and the shot on each audio line |
| A frozen listener | give them the alive line and one action in the silence |
| The voice differs from the cast voice | the recording isn't wired, or the model re-voiced: wire it, and say "use the attached recording as the voice" |
| The reference's set, pose or headgear copied in | its ignore line, or an OVERRIDE, and a count of zero for the thing |
| Faces drift across shots | wire the video sheet as well as the image sheet; fewer shots in the part |
| A dark scene comes out lit | name the sources; "no ambient fill"; the darkest and brightest thing of each frame |
| Extra tears, smiles, people, objects in hands | a count for each |

Change one thing per retry, as a new take.

## 7. From part to part

- **The same input map in every part**: the same nodes wired, the same roles in the same words, with `context
  only` on what this part doesn't show. Copy the REFERENCES, WHERE EVERYTHING IS and LIGHT sections between
  parts; change only the shots, the lines and the counts.
- **Screen direction is one decision for the scene**, in the scene's continuity note, repeated in each part.
- **Continuing from a clip**: when a part must begin exactly where the last one ended (a line answered in the
  same breath), cut the last 3–4 s of the picked take into a file (`ffmpeg -sseof -4 -i <take> -c:v libx264
  -crf 16 -c:a aac canvases/<ep>/assets/p2a-tail.mp4`), add it as an asset node, and wire it into the next
  part's video card on the model's extend mode. The prompt calls it SHOT 0: "keep SHOT 0 exactly as it is and
  continue directly after it with a hard cut into SHOT 1; times below count from the start of SHOT 0". The
  next part's `Cut:` line then starts after SHOT 0 (`in 4.0`), so `assembly` doesn't play it twice.
- **Re-running a part** after a recording or an image changed: keep the prompt and swap the wire; update the
  start times and phrase times from the new recording before running.

## 8. The cut

Run each part two or three times. Review every take next to the parts around it and keep only what works: the
opening of one generation, the answer from another, the look from a third. Write them as `Cut:` lines in the
part's note, in order, each naming its take (`shot-listing` §3); `assembly` joins the passages. Cut on a blink,
a turn of the head or to the other person. The recordings stay the reference for the words: when a take's
voice drifted, `assembly` lays the recording under the picture instead (the `Audio:` line).

## Pitfalls

- Generating picture first and fitting voices afterwards.
- A whole conversation in one generation.
- A reference wired without a role, or with a role and no "ignore".
- Beats without times, or times guessed instead of measured.
- Writing only the speaker.
- Prohibitions where a count would do.
- Judging by the first frame: check the silences and the listener.

## Next

What each person plays → `acting-direction`. Voices, tags, effects, the mix → `sound-design`. Modes and model
wording → `video-prompting`. Joining passages → `assembly`. Physical action in the scene →
`action-choreography`.
