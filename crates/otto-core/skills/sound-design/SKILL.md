---
name: sound-design
description: Making an episode's sound: dialogue lines in each character's voice from their entity file (generate_audio speech with delivery tags), sound effects, music cues, when to use a video model's native audio instead, lip-sync options and their limits, voice and sound cards in the shot frames (wired into the video card or mixed later), and the levels and ducking notes handed to assembly. Load after a scene's videos exist (or while they render), when the user asks for dialogue, voices, SFX, ambience or music, or when a talking shot needs a lip-sync decision.
---

# Sound design

Sound is half the episode and the cheapest half to get right. Work scene by scene from the shot notes
(`Dialogue:` and `Sound:` lines, written by `shot-listing`). Everything you make is a card on the canvas:
Speech and Sound cards in the shot frame, ambience and music in the scene frame. The shot note's `Audio:` line
and the Mix note tell `assembly` what to lay where.

## 1. Before generating

1. `canvas_read {canvas}`: shot order, notes, which video cards were run with sound on (their options) or have
   `audio` wires, and any voice or sound cards the user already made.
2. For each speaking character, `read` the entity file: `voice: <provider>/<voice>` (e.g.
   `elevenlabs/JBFqnCBsd6RMkjVDRZzb`, `openai/marin`). No voice yet: cast one with `character-design` §5
   before writing any line; never improvise a voice per line.
3. `list_media_models {kind: "audio"}`: speech models (ElevenLabs v4 and v3 take audio tags; Multilingual v2 is
   steadier for long narration; OpenAI TTS here takes text only), `sound-effects-*` (0.5–30 s), `eleven-music-*`.

## 2. Decide per talking shot: native audio, separate speech, or lip sync

Separately generated speech will not match mouths that a video model animated. Decide before generating video
when possible (it changes `sound` and the prompt):

| Situation | Do |
|---|---|
| A scene of talk, and a video model that takes reference audio (Seedance 2.5 does) | **record every line first**, wire the Speech cards into the part's `audio` port and time the picture to them: the model lip-syncs to the cast voice (`dialogue-scenes`). The default for dialogue |
| Mouth not visible: back of head, wide, profile in shadow, O.S., V.O. | separate speech (§3); the clean, consistent default |
| Speaker on-screen, short line, model with native dialogue | native audio in the video prompt (`video-prompting`), with the voice wired in where the model takes one |
| Speaker on-screen, voice must match other shots exactly | separate speech, then a lip-sync card over the clip (§5) |
| Nothing above works | cut away: play the line over the listener's reaction or an insert |

Native audio voices differ from the entity voice and from shot to shot. Bind the voice where the model allows:
Seedance and Wan take audio references (`Image 1 is Mara and uses the voice timbre from Audio 1`; Wan: `Voice
timbre references Audio 1`): wire an asset node of `world/characters/<id>/voice-sample.mp3`, or the line's
Speech card, into the video card's `audio` port. Kling binds a voice to an element; Grok takes preset
`voice_id`s only. Otherwise keep native audio for ambience and use separate speech for words.

## 3. Dialogue lines

One Speech card per line (or per uninterrupted run of one speaker), in the shot's frame, titled by shot,
speaker and index (`S2.3 mara-01`). The one-call way pins the voice from the entity file:

```
generate_audio {type: "speech", provider: "elevenlabs", voice: "JBFqnCBsd6RMkjVDRZzb",
  text: "[quietly, tense] Not again…", canvas: {name: "ep01-pilot", frame: "<S2.3 frame id>"}}
```

Then title the cards in one `canvas_edit` (`update_node {id, body: {title}}`, ids from the results). Or build
them as drafts with `canvas_edit` (`makes: "speech"`, the line as `prompt`, the voice in its options) and
`canvas_run` the scene's lines at once. Takes land in `assets/`; audio you cut or process yourself goes in
`assets/audio/`. Then either:
- **Wire** the card into the shot's video card (`audio` port) when that model voices or lip-syncs from it;
  the line is then in the clip's sound.
- **Leave it unwired** beside the video card, to be mixed in `assembly`: list it in the shot note's `Audio:`
  line (§7).

Writing the text (ElevenLabs v4/v3 practice):
- **Tags** in square brackets before the words they colour: emotion and delivery (`[whispers]`, `[sighs]`,
  `[sarcastic]`, `[curious]`, `[excited]`, `[crying]`, `[laughs]`, `[exhales]`). Describe the voice quality
  plainly (`[low, gravelly voice]`, `[quietly, controlled fear]`): an ambiguous tag can come out as a sound
  effect. Match tags to the voice: a calm narrator won't shout convincingly.
- **Punctuation** steers delivery: ellipses for hesitation and pauses, a dash for a cut-off, CAPITALS for
  stress. v3/v4 ignore SSML `<break>`; use `[short pause]`, ellipses or sentence breaks.
- **Spell out** numbers, dates, units and abbreviations as spoken ("twenty past six", "Doctor Okafor").
  Unusual names: v4 takes IPA in slashes (`"/ˈmɑːrə/"`); otherwise respell phonetically.
- Words only from the script. Stage directions outside brackets get spoken.
- Models without tags (Multilingual v2, OpenAI here): carry the emotion with wording and punctuation only.

After each line: `ffprobe -v error -show_entries format=duration -of csv=p=0 <take>`; for a line that picture
will be timed to, also where its pauses fall (`dialogue-scenes` §2 has the `silencedetect` line). A line longer than its
shot (minus ~0.3 s air) means: shorten the line with the user, lengthen the shot, or let it run over the next
shot as a J/L cut (write that in both shot notes). Run the cards of lines that carry the scene twice and pick
the better take.

## 4. Sound effects and ambience

Sound cards: `generate_audio {type: "sound", text, duration?, canvas: {name, frame}}`, one sound per card. A
shot's SFX goes in its shot frame (`S2.4 sfx-bell`); a scene's ambience in the scene frame (`Sc2 amb-harbor-fog`).

- Describe the action, the material, the space and the performance, not a category: `A large bronze bell tolls
  once far across open water in fog, long decaying hum` beats `bell sound`.
- Sequences in order with "then": `Footsteps on a wet wooden dock, then a cabin door creaks open`. Exact counts
  ("knocks 4 times") are unreliable; generate one and repeat it in `assembly`, or accept the take.
- Useful terms: impact, whoosh, ambience, one-shot, loop, drone, braam, glitch, room tone.
- Ambience per scene: one bed of 20–30 s (the model's maximum is 30 s) with no events in it (`steady harbor
  ambience at dawn: soft water lapping, distant gulls, no voices, no music`); `assembly` loops it under the scene.
- Set `duration` for one-shots that must fit a moment; leave it out to let the model choose.
- When a video was made with native audio and its ambience is good, use it and skip the bed for that shot.

## 5. Lip sync

Where the catalog has a lip-sync mode (`list_media_models {query: "lip"}`): e.g. `kling-lipsync-audio-to-video`
takes `video_url` (2–10 s, 720–1920 px per side) and `audio_url` (2–60 s, ≤ 5 MB); some MiniMax H3 endpoints
have `lip-sync/image-to-video` from a still. Read the schema and make the line first. Then a second video card
in the shot frame with that model (title `S2.3 video sync`), wired `video` ← `S2.3 video` and `audio` ← the
Speech card; `canvas_run` it. It is now the end of the shot's video chain, so `assembly` cuts its picked take.
Check the result with a frame strip (`continuity` §5): lip sync often softens teeth and jaw. Say plainly when a shot can't be synced
and offer the cutaway.

## 6. Music

`generate_audio {type: "music", text, duration, canvas: {name, frame}}`; runs in the background. One Music card
per scene or per stretch of scenes, titled `M1 harbor-dawn`, in the first scene frame it plays under.

Answer the five questions the model otherwise answers for you (ElevenLabs music guide): genre, mood,
instrumentation, tempo (BPM) and era/production, plus key when cues must join. Say `instrumental only` or it
writes lyrics. Narrate the arrangement in time, marking the silences:

```
Sparse dark-folk underscore, 72 BPM, D minor, instrumental only. Start with just a low bowed cello drone;
add a slow fingerpicked nylon guitar at 15 seconds; a single bell-like celesta note on each bar from 30 seconds;
no drums. Close-mic'd, dry room. End cleanly on a held D at 58 seconds.
```

- `duration`: the cue's length plus ~2 s for fades. Under dialogue: no lead vocal, a sparse midrange (cello,
  pads, low piano), so speech stays clear.
- Get the scene lengths from the shot notes' cut durations; music written to them needs no stretching.
- **Action cut to music** (fights and chases, above all in animated shows) needs the cue *before* the picture:
  make it first, state its BPM in the prompt and in the Mix note, and give `action-choreography` the beat
  length (60 ÷ BPM seconds) so hits and cuts are planned on beats (`animation-direction` §7).

## 7. Hand-off: notes on the canvas

`assembly` takes each card's picked take; the notes say where it goes. In each shot note, an `Audio:` line for
the unwired cards to mix under that shot, by card title, with the offset from the shot's start (seconds) and a
role (`dlg`, `sfx`). Lines wired into the video card are in its clip already; leave them out.

```
- Audio: S2.3 mara-01 @0.6 dlg · S2.3 sfx-rope @1.2 sfx
```

One `**Mix · ep01-pilot**` note outside the frames holds the rest:

```markdown
**Mix · ep01-pilot**
- Music: M1 harbor-dawn from S2.1 +0.0 to end of S2.5, fade in 1 s, out 2 s
- Ambience: Sc2 amb-harbor-fog under all of Scene 2, loop
- Clip sound: keep S2.1 and S2.4 (water); mute S2.3 and S2.5 (separate dialogue). Keep any clip whose line
  was wired into it.
- Levels: dialogue reference; SFX 6–10 dB under; ambience 18–22 dB under; music 15–20 dB under, ducked a
  further 8–10 dB while anyone speaks
- Target: -14 LUFS integrated, -1.5 dBTP (web, social); -16 LUFS for a podcast-style or talky cut
```

## Pitfalls

- A different voice id for the same character in another episode: always read it from the entity file.
- Emotion in narration text ("she said sadly") with tag-less models: it is read aloud.
- Music with lyrics under dialogue; music bed shorter than the scene.
- SFX carrying ambience and events in one file: separate them so assembly can level them.
- Promising lip sync that the catalog can't deliver; say so and cut away instead.

## Next

The mix and the cut → `assembly`. Re-timing shots for a line → `shot-listing`. How a line is played on the
face → `acting-direction`. Scenes built on recorded lines → `dialogue-scenes`.
