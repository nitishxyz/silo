---
name: animation-direction
description: Directing motion for a Studio whose medium is not live action - painterly 3D in the vein of Arcane, comic-book 3D, 2D anime and cel, stop-motion - writing the motion line of world/style.md that every video card gets, key poses and timing, stepped motion and smears, drawn effects, a designed camera, impact inserts as their own shots, and keeping video from drifting to glossy 3D or live action. Load when style.md names an animated or stylised medium, before video cards in such a studio, for animated fights and effects, and when clips look like a game cutscene, plastic, photoreal or too smooth.
---

# Animation direction

In a stylised show a still can be exactly right and the clip made from it still wrong: the model animates it
like filmed reality (even, floaty, motion-blurred) or re-renders it as glossy 3D. The style line says how a
frame looks. An animated show also needs to say **how things move**, and that is what this skill adds. Load
`style-guide` for the look itself and `video-prompting` for modes.

Models differ a lot here. Treat every rule below as a starting point: test the motion line on one 5 s shot,
look at the frames, and adjust before a scene depends on it.

## 1. Evidence, not titles

"In the style of <show>" leaves the model to guess which of the show's properties you mean, and some providers
refuse titles and studio names. Write what is on screen. Keep the title as a note for people in `style.md`.

| Family | What to write (pick what your show has) |
|---|---|
| Painterly 3D | solid 3D forms carrying hand-painted texture; visible brush strokes on skin and cloth; faces cut into flat planes of painted shadow; matte surfaces, nothing glossy; backgrounds that are paintings with depth; smoke, dust, sparks and fire drawn as flat hand-made shapes over the scene |
| Comic-book 3D | ink outlines on the forms; halftone dots in the shadows; flat colour with slightly offset edges; graphic bursts and lines drawn into the frame at impacts |
| 2D anime, cel | clean line art; flat colour with one shadow tone; held drawings where only hair, cloth or the mouth moves; speed lines; painted backgrounds that slide behind the characters |
| Stop-motion | real materials (felt, clay, wire) at miniature scale; fingerprints and a slight shimmer between frames; practical miniature lights; motion in small distinct steps |

The same goes for motion: "animated like <show>" is a title. §3 has the words.

## 2. The motion line

`world/style.md` gets a `## Motion line` section beside the style line. A run appends it, after the style
line, to every **video** card (not to images), unless the card's style is off. So a video card's prompt never
repeats it, and a direct call made with `path` ends with the style line and then the motion line.

```markdown
## Motion line
Hand-keyed stylised animation: bodies snap between clear held poses with weight and follow-through in hair and
cloth; the painted brush texture stays fixed to the surfaces as they move; smoke, dust and sparks are flat
hand-drawn shapes that change in steps; a matte painted finish in every frame.
```

- 25–45 words, one paragraph, states rather than instructions, no titles, nothing negative.
- It names four things: how bodies time their movement, what the surface texture does while moving, how
  effects are made, and the finish that must survive.
- For a flat-shaded look, say how light behaves on a shape: "each colour shape keeps one flat tone that steps
  lighter or darker as a whole as it turns from the light, crisp edges, no gradient inside a shape". Left
  unsaid, video adds soft 3D gradients to a look that has none.
- Change it only in `style.md`. A change reaches every video card's next run; nothing is marked Out of date
  (`style-guide` §5).

Below it, `## Motion` holds the long form you consult per shot: timing, camera, effects, impact language, and
a "steer away from" list written as pairs (what drifts → what to write instead).

## 3. Timing: poses, not flow

Animation reads through **key poses**: clear silhouettes that are held, with fast movement between them.

- **Write the poses and the hold.** "She lands in a low crouch and holds it for half a second, then is across
  the gap in a blink and standing over him." The hold is what lets the pose read as a drawing.
- **Anticipation, named for the action it belongs to.** "A held breath before she presses the button", "the
  hull dips a few centimetres before the arms come out". For a *strike* write it as a held state and the blow
  as already happening ("coiled low and still, weight on the back foot; then the strike is already landing"):
  "winds up" makes a model spend the beat telegraphing (`action-choreography` §3).
- **Settle after.** Hair, cloth, a hanging strap arrive a moment after the body and swing to rest. Name which
  parts follow through; it is most of what makes weight read.
- **Stepped motion: the term, what it means, and what it applies to.** "Character animation on twos: every
  pose held for two frames, twelve poses a second, a crisp stepped cadence, while the camera, the effects and
  the light changes stay smooth." The term alone is a coin toss, and stepping everything makes the camera
  judder; the split is what reads as hand animation. For a hold: "each state arrives quickly and is then held
  completely still until the next". For speed: "fast limbs stretch into a painted smear for an instant".
- **The principles, attached to things.** "Twelve principles of animation" as a phrase does little. Say where
  each one acts in this shot: slow-in and slow-out on eye shifts and head turns; an arc on the hand's path to
  the switch; overlapping action on the fringe, the straps and the sleeve; squash and stretch in brows, lids and
  lips only; secondary action in the cabin's constant faint vibration. Three or four per shot, the ones it has.
- **Uneven tempo.** Fast, hold, fast. Even speed from start to end is what makes a clip look simulated.
- **One clear action per shot**, shorter than in live action: 2–4 s for a single shot, with the pose at its end
  described so the cut lands on a strong picture.

## 4. Camera

Decide once, in `## Motion`, which camera the show has, and keep to it:

- **A virtual film camera** (most painterly and comic-book 3D): real lens framing, shallow focus, push-ins,
  a short shake on a hard impact.
- **A graphic camera** (most 2D): locked frames, snap zooms, pans across a wide background painting, the
  background streaking sideways while the character holds.

For a turntable or an orbit, fix the light to the set ("one soft key from the front-left, fixed to the set,
so the shaded side travels across her as the camera goes round"): light that follows the camera flattens the
form the orbit is there to show.

Either way, leave out the *artefacts of a physical camera* (film grain, lens dirt, an operator's breathing,
rolling-shutter wobble, lens brand or film stock names) unless the style frames have them: they pull the whole
render toward live action. Describe the move with the camera words in `video-prompting` §3.

## 5. Effects and impacts

- **Effects are drawn.** Say what shapes they are made of: "a burst of dust in flat rounded shapes", "sparks as
  short bright streaks", "a flat white ring expanding from the point of contact". Unqualified smoke, fire or
  water comes back photoreal and breaks the frame.
- **Force shows in the picture, not in injury.** A one-frame flash of flat colour, a shock ring, a dust burst
  under the feet, the background streaking. This also keeps a no-gore rule without writing a prohibition.
- **Impact inserts** are their own shots, never blended into the scene shot (the insert's look leaks into the
  rest of the clip). 0.3–0.6 s, a hard cut in and a hard cut out, on a flat background of one or two colours
  with no location in it:
  - a held frame of the moment of contact, silhouettes only;
  - the same frame with its colours inverted, for a single beat;
  - the striker's eyes, wide across the frame;
  - a rush of speed lines toward the point of contact;
  - a shock ring on flat colour.
  At most two in a fight. The dependable way to make one is a still: an image card in its own shot frame,
  plan `still`, held 8–14 frames by `assembly`. Its note carries the cut it sits on.
- **Light and colour can jump** at an impact (a flash frame, a palette shift for one shot) when the show's style
  allows it. Write it in `## Motion` so it is a rule, not an accident.

## 6. Keeping the look in video

In order of strength:

1. **Image-to-video from an approved keyframe.** The keyframe carries the rendering; the prompt then describes
   motion only. This is the default for stylised shows, more than for live action.
2. **A style frame as a reference** on reference-to-video and on long passes, with one job: "Image 5 is the
   rendering only: its brush texture, shading and finish. Not its people, place or light." Use a frame from
   the same place when there is one.
3. **The motion line.**
4. **Shorter clips and less camera travel.** Texture swims and faces go smooth when the camera moves a lot.
5. **Remove photoreal triggers** from the style line and the prompt: realistic, photorealistic, cinematic film
   still, film grain, skin pores, lens and stock names. Check `style.md` for them once.

Then look. Grab the first, middle and last frame and `read` them beside the keyframe (`continuity` §5 has the
`ffmpeg` line): is the paint still on the surfaces, are the shadow shapes still flat, has the face gone smooth?

| The clip looks | Change |
|---|---|
| Like a game cutscene, plastic, glossy | start from a keyframe; add "matte painted finish"; remove gloss and realism words |
| Too smooth, floaty | held poses with lengths; uneven tempo; stepped-motion wording; a shorter clip |
| Like live action from the middle on | a style frame as reference; less camera travel; shorter duration |
| Texture crawling or flickering | "the painted texture stays fixed to the surfaces"; slower, smaller camera move |
| Real smoke, fire or water | describe the effect's shapes as flat and hand-drawn |
| A different face by the end | wire the sheet as a reference alongside the keyframe (`continuity` §2) |

## 7. Animated fights

Load `action-choreography` and keep all of it: beats with reactions, geometry in place of terms, positive
locks, the place's sides, passes. Then change four things:

- **Fewer, clearer beats.** Hold the key pose of each exchange; cut the beat budget by about a third.
- **Silhouettes at contact.** Profile or three-quarter, the two bodies apart enough to read as two shapes,
  against the lightest part of the background.
- **Force through drawn effects and inserts** (§5) instead of marks on the body.
- **Music first.** Stylised action is cut to its track. Before the pass, have `sound-design` make the cue, note
  its tempo, and put hits and cuts on the beat: at 120 BPM a beat is 0.5 s, at 90 BPM 0.67 s. Write those times
  into the choreography table's Time column, and keep the pass's own sound to impacts and breath.

## Pitfalls

- A title standing in for the style or the motion.
- "On twos" with no description, or applied to the whole frame: say what is stepped and what stays smooth.
- Camera artefacts and realism words in a stylised show.
- Inserts blended into the scene shot.
- A motion line rewritten per prompt: it drifts like any style text. Change it in one place.
- Judging from the keyframe: the still was never the problem. Look at frames of the clip.

## Next

The look and the style line → `style-guide`. Fights → `action-choreography`. Modes and per-model wording →
`video-prompting`. Faces → `acting-direction`. Inserts held as stills, cuts on the beat → `assembly`.
