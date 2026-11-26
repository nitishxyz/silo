---
name: series-writing
description: Writing the story of a Studio series - the series arc and engine, season outline, episode outlines, and each episode's canvases/<episode>/script.md with scene headings, visible action, dialogue, rough timings and an end hook sized to the format (a 90 s vertical episode is not a 10 min one) - plus a five-question check of whether a scene works (goal, obstacle, tactic, turn, what the audience now thinks), continuity between episodes and keeping world/ in step. Load when planning a series or season, writing or revising an episode script, auditing a scene that feels weak, or deciding what happens next.
---

# Series writing

The script is where cost and consistency are decided: every scene, place and person you write is something to
generate and keep consistent. Write for what the screen and the models can carry. Read `world/overview.md`, the
characters involved and the previous episode's script first.

## 1. Series arc (in `world/overview.md`, `## Series`)

Write it before the first script, short enough to hold in your head:

```markdown
## Series
**Engine:** Every dive brings up something from the drowned town that someone wants buried.
**Arc:** Mara searches for her father and learns the Guild drowned the town on purpose; by the end she must
choose between exposing them and saving him.
**Leads:** Mara wants her father back; the Guild's control of every dive stands in her way; she believes
she is only a diver, not someone who leads. Teo wants to leave the port; loyalty to Mara keeps him.

### Episodes
1. The Bell - Mara finds her father's diving bell, still warm. Hook (identity): his voice on the bell's radio.
2. Undertow - The Guild seizes the bell; Mara dives illegally. Hook (emotion): Teo refuses to cover for her.
3. …
```

- **The engine** is what makes episode four possible without coincidences. If you can't name it, fix the
  premise before writing scripts.
- **For each lead**: what they want, what stops them, the lie they believe about themselves, how they change
  by the end and how they don't. Write it in their character file too (`character-design`).
- **Episode loglines** one line each with the end hook and its type. Outline the whole first block before
  scripting so the hooks rotate and the reveals land where they should. Give direction for later seasons,
  not a beat map.
- **Show range**: episodes should differ in pairing, place and kind of problem while coming from the engine.

## 2. Size to the format

Spoken pace is about 3 words a second: count the dialogue and voice-over, not the action, to time a scene.

| Format | Episode | Scenes | Spoken words | Shape |
|---|---|---|---|---|
| Vertical drama (9:16) | 60–120 s | 2–4 | 180–360 | hook in the first seconds, a turn every ~30 s, end on a cliffhanger |
| Web episode (16:9) | 3–12 min | as the story needs | ~3 words/s of talk | open on the problem, end on a question |
| Short film | 3–15 min | as the story needs | ~3 words/s of talk | one complete arc |

**Vertical episodes**, beat by beat. Draft a timestamp skeleton before any dialogue:

```
0:00 Hook       the conflict itself, on screen in the first seconds; a stranger understands it without backstory
0:15 Friction   two people in one place under pressure: one is lying, hiding or about to break
0:45 Turn       new information, a shifted allegiance or a raised danger
1:10 Spike      the biggest jolt; it re-prices what came before (it must work with the sound off)
1:25 Button     cut on the question, not the answer: a reaction close-up, then black
```

- **End hook types, rotated** so no two in a row match: identity (someone is not who they said), emotion (a
  confession, refusal or rupture left hanging), realisation (an earlier judgement was wrong), arrival (someone
  walks in who changes everything), interruption (cut mid-action). Label each in the script.
- **End on the reaction**, not the reveal: the last action line is the face of the person it hits hardest.
- **Two or three central characters.** Vertical frames hold faces; write reaction beats out (what the eyes and
  hands do) instead of "(she reacts)".
- **Dramatic irony** carries short thrillers: let the viewer know what the lead doesn't by episode 2 or 3.

## 3. Write for the models

Video models are reliable on one- and two-person dialogue close-ups, single-character emotional beats, hard
light, rain and snow, car interiors. They are unreliable on more than four people in frame, crowds, fine hand
work, complex machinery and readable text in close-up (contracts, phone screens). Write around the second
list: a crowd becomes sound and two faces; a letter becomes a voice reading it.

**Fights, chases and stunts** work when they are planned, not improvised: `action-choreography` turns them
into timed beats and generates them as passes. In the script, write a fight as its turning points in three to
five `[ACTION]` lines: who starts it, where the upper hand changes, what breaks, how it ends and what it costs
the winner. Keep it between two people where the story allows, in a place whose layout is written down, and
give it a length in the scene heading; 10–30 s of continuous action is a lot of screen time. With only
short-clip models connected (`list_media_models`), keep to the moment before and the aftermath.

Keep each episode to 3–4 places, reusing world places (`location-design` before inventing one).

## 4. `script.md`

Create the episode canvas first: `canvas_create {title: "The Bell", episode: 1}`; it returns the folder name
(`ep01-the-bell`). Write `canvases/ep01-the-bell/script.md`:

```markdown
# Ep 1 · The Bell
Logline: Mara finds her father's diving bell, still warm.
Runtime target: 90 s · Places: places/old-bell-quay, places/mara-flat · Characters: characters/mara, characters/teo

## Scene 1 · EXT. OLD BELL QUAY - DAWN (~20 s)
[ACTION] Mara hauls a winch chain hand over hand. A brass diving bell breaks the green water, barnacled,
porthole fogged from inside.
TEO (off, from the steps): Leave it. That's Guild salvage.
MARA (not turning): It's warm.
[ACTION] She presses her palm to the brass and pulls it back fast.

## Scene 2 · INT. MARA'S FLAT - NIGHT (~40 s)
…

[HOOK · identity] The bell's radio crackles. A man's voice: "Mara?" Close on Mara's face: she knows the voice.
```

- **Scene heading**, fixed order: number, `INT.`/`EXT.`, the place's exact world name, time, and the scene's
  rough length. Use the same place name as `world/places/` every time.
- **`[ACTION]` lines**: only what can be seen or heard, under ~40 words each, present tense. Light by its source
  ("an oil lamp on the table, frame left"), not by its effect. Name only the people in the scene; writing who
  is absent ("Teo is gone") tends to bring them into the picture.
- **Dialogue**: `NAME (delivery): line.` Lines under ~20 words; they become captions. No "she said".
- **Timing**: after the draft, count spoken words per scene (÷ 3 = seconds), add a few seconds per action beat,
  and check the total against the target. Cut lines, not hooks.
- **Hook** labelled with its type at the end.

Pin it on the canvas as a document card, top-left and outside the frames (once; it follows the file when you
rewrite it): `canvas_edit {canvas: "ep01-the-bell", ops: [{op: "add_node", node: {x: 0, y: 0, body: {kind:
"asset", path: "canvases/ep01-the-bell/script.md"}}}]}`. Don't restate the script in a note. Then show it to the
user (`studio_open {canvas: "ep01-the-bell"}`) with a two-line summary ("the script is pinned on the canvas, top
left") and go on to `shot-listing` unless they want changes.

## 5. Check each scene

Before a script goes to `shot-listing`, test every scene (and when the user asks whether a scene works, or
what is weak in it) against five questions, in this order. Answer each in one line; the first one you can't
answer is what to fix.

1. **Goal.** What is the lead trying to get in this scene, and is it a link toward what they want in the
   episode? Take the scene out: if the story still holds, the scene has no goal.
2. **Obstacle.** What puts that at risk here, and is it this step or the whole aim that is at risk? A thing
   that only costs effort is not an obstacle. Nothing at risk: the scene goes slack.
3. **Tactic.** What does the obstacle force them to try? It should be a reasonable guess on what they know
   (they can be wrong about how, never about what they want), and when it fails it must tell them, and us,
   something that changes the next move. A failure that teaches nothing is a wheel spinning.
4. **Turn.** Where does the situation go against expectation: their own move works against them, or someone
   they were acting on turns out to have been acting on them? A single scene may only escalate, but every
   sequence (from a danger opening to it being settled) needs at least one turn.
5. **Re-read.** What did the audience think of this character before the turn, and what do they think after?
   Name both. A turn that flips the plot and leaves that opinion where it was does nothing; it is the most
   common weakness, so look for it first. Across an episode the opinions should deepen, not just flip back and
   forth.

When fixing, offer the smallest change first (only the weakest point, the rest of the scene as written), then a
freer rewrite if the small one isn't enough. `acting-direction` works from the same scene one level down: what
each person does to the other, line by line.

## 6. Between episodes

- Open each episode where the last one's hook left the viewer: answer it in the first scene or raise the stakes
  on it, never ignore it.
- Track what changed: wounds, costumes, who knows what, where objects are. Write them to the world in the same
  turn (dated lines in the character or place file, events in `## History and timeline`), so `continuity` can
  carry them into shots.
- Keep `## Series` current: when an episode goes somewhere new, update the loglines after it.
- A new recurring character, place or prop: create the entity file now (`worldbuilding`); design it when it is
  first shown.

## Pitfalls

- A TV pilot cut into 90-second pieces: vertical episodes are built backwards from the hook.
- Setup before conflict: if episode 1 needs a minute of world before the problem, restructure the premise.
- Over-explained cliffhangers: cut two seconds earlier than feels safe.
- New places and extras in every episode: each one is generation work and a consistency risk.
- Story facts that exist only in a script: put them in `world/`.

## Next

Script done → `shot-listing`. New characters or places it needs → `character-design`, `location-design`.
A fight, chase or stunt in it → `action-choreography`. Dialogue voices and music cues → `sound-design`.
