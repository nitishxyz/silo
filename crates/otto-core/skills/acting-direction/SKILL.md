---
name: acting-direction
description: Directing a character's performance in a Studio shot - the scene's shared direction, each character's motive, goal, obstacle and tactic, written as a performance block in the video prompt with verbs aimed at the partner, the eyes given something to check, and beats keyed to the dialogue words, plus takes and picking passages. Load for dialogue scenes, reaction shots, close-ups and any shot whose point is a face, before their video cards, and when a take's acting came out dead, glassy, frozen, flat, generic or over-acted.
---

# Acting direction

A model plays what the prompt describes. Describe the result ("she looks sad", "his brows lift", "nervously")
and you get a mask: a face holding an expression with nobody behind it. Give the character something to do to
the other person, and something to find out from their face, and the expression arrives by itself and keeps
changing. This skill is how to work out that task and write it down.

Load `video-prompting` for modes and per-model dialogue syntax, `sound-design` for whether the line is spoken
in the clip or made separately.

## 1. Work it out per scene, in this order

Read the **whole** scene in `script.md` before directing any line of it, and read how it ends first: the last
line or look usually says what the scene was about, often with a second meaning.

1. **The scene's direction**: one line that everyone in the scene is playing toward, mostly unspoken; an
   agreement about how this moment will be lived. Two sisters clear their father's flat the day after the
   funeral: *get it done, nobody mentions him*. It belongs to both of them at once. It is not the plot point
   the scene delivers; characters don't know what the film needs from them.
2. **Everyone is inside it.** If a character in the scene (a silent one, a sleeping one) stands outside the
   line you wrote, the line is wrong. Rewrite it until each person is a different face of the same thing.
3. **The activity**: what they are physically doing (packing, coiling rope, doing the rounds). Each character
   pursues the scene's direction *through* it, in a way of their own. The camera can only read the invisible
   through something visible.
4. **Per character**:
   - **Motive**: why this person pushes in that direction. Same direction, different reasons: the elder keeps
     it brisk because she is the one who has to hold together; the younger because if she stops she won't start
     again. Take it from the character file and from what they have already done: someone who took the Guild's
     money can't play an innocent.
   - **Goal**: what they are fighting for in this scene, personal and ordinary. Never the theme.
   - **Obstacle**: what presses against the scene's direction (the real feeling, the thing nobody says). One
     slip and the line breaks. Nobody plays the obstacle; everybody plays keeping it out. That pressure is what
     the audience feels.
   - **Tactic**: what they do **to the other person**, moment to moment, and what they check in the other's
     face to see whether it worked.

Write it as a `**Performance · Scene N**` note in the scene frame (beside the continuity note), five or six
lines. Every performance shot of the scene is written from it:

```markdown
**Performance · Scene 1**
- Direction (shared, unspoken): it's only salvage. Nobody says whose bell it is.
- Activity: Mara works the winch; Teo stays on the steps with his mug.
- Mara: motive, if Teo sees she is afraid he will stop her diving. Wants him gone before she opens it.
  Tactic: keeps her hands busy, answers the bell not him, checks him sideways to see if he bought it.
- Teo: motive, he promised her father. Wants one reason to believe her. Tactic: says the rule out loud and
  watches her hands, not her face, to see what they do when he says "Guild".
- Against them: the bell is warm. Either of them saying why ends the pretence.
- Breaks at: "It's warm." She says it to the bell and it comes out as a question.
```

**Pairs.** A two-person scene is richer when the pair is built on contrast: give each a strength and a flaw,
mirrored against the other, and decide which one opposition the audience should read (who actually cares about
the bell). A surface trait often covers its opposite (the one who seems not to care stopped hoping long ago):
direct what happened to them, not how they seem.

## 2. The performance block

Add one block per character on screen to the shot's video prompt, after the shot, camera and light:

```
PERFORMANCE - MARA (her attention stays on Teo for the whole shot; the work shows in her eyes)
Scene direction (shared, unspoken): it's only salvage; nobody says whose bell it is.
Why she holds to it: if Teo sees she is afraid, he stops the dive.
She wants: him gone before she opens the hatch.
Against her: the brass is warm under her hand.
What she does to him: keeps working, answers without turning, then checks his face to see whether he believed
her.
Beat by beat:
- "Leave it." (Teo, off): her hands keep hauling; her eyes go to his reflection in the porthole.
- silence, about a second: she reads whether he is coming down the steps. He isn't. She hauls faster.
- "It's warm.": said to the bell. Then one look straight at him: did he hear what that means?
Her eyes stay on what she is checking, with ordinary blinks.
```

Rules:

- **Verbs aimed at the partner**: checks, tests, hides from, waits for, measures, dares, gives up on. No adverbs
  of feeling (sadly, nervously, angrily) as instructions.
- **No face choreography**: no "brows lift", "lip trembles", "eyes widen". The one exception is the last line of
  the block, which keeps a model from freezing the stare.
- **The eyes get a job**: what are they looking for in the other person, and what do they do when they find it
  or don't? A person arguing from conviction keeps checking whether it is landing; a person hiding something
  looks, and looks away before being caught.
- **Beats are keyed to the words** of the dialogue, quoted exactly, with silences given a length.
- **Mark the break** where the script has one: the moment the character's reason runs out and the feeling shows
  for a second. It is the scene's delivery; everything else is holding it back.
- **The listener has a task too**: deciding whether he is serious, waiting for the catch, protecting the mood.
  A listener without one stares.
- **Nobody plays the emotion.** Everyone plays the scene's direction; the audience gets the feeling from the
  pressure against it.
- Keep it to the beats that matter. A block longer than the shot can hold gets averaged into a blank face.

In the shot note add `- Acting: <the tactic in one line>`, so the plan shows on the canvas (`shot-listing`).

## 3. Making performance shots

- **Size and length**: singles and over-the-shoulders, medium close-up to close-up, one small action, little or
  no camera move. Let the shot run long enough for the silence (a line plus one to two seconds).
- **The keyframe's face is at rest.** A keyframe (or a sheet) with a peak expression gives every frame of the
  clip that expression. Start from the character's resting face and let the block move it.
- **Lines**: with a model that speaks in the clip, write each line with its speaker as `video-prompting` shows
  for that model, and keep the performance block's quotes identical. With separate speech, the block still
  drives the face; `sound-design` decides lip sync.
- **Takes**: performance varies from take to take far more than framing does. Run 3–4 takes of a shot that
  carries the scene. You can't see video: grab a strip of frames around each beat (`continuity` §5) and compare
  the eyes at the break.
- **Pick passages, not takes**: the best first half and the best look may be in different takes. Write several
  `Cut:` lines in the shot note, in order, each naming its take; `assembly` joins them. Cut on a blink, a turn
  of the head or to the other person, so the join doesn't jump.

## 4. Fixes

| The take shows | Change |
|---|---|
| A glassy or frozen stare | give the eyes something to check; add the blink line; give the listener a task |
| Mugging, over-acting | remove feeling adverbs and face choreography; lower the stakes of the tactic ("tries not to") |
| The same expression from first frame to last | the keyframe or sheet carries it: remake from a resting face |
| Eyes wandering at random | name who or what they go to at each beat |
| Flat delivery of the line | key the beats to its words; add the silence before or after it |
| A generic "emotional" face | the scene's direction is missing or is really the plot point: rewrite it |
| Two characters acting alike | their motives are the same: give each their own |

## Pitfalls

- Directing from one line instead of the whole scene.
- Giving a character the film's purpose ("she reveals the theme") as their task.
- Confusing the scene's direction (shared) with a character's goal (personal).
- A motive the character's history doesn't allow.
- Fixing dead eyes with lighting words (catchlights, sparkle) instead of a task.

## Next

A whole scene of talk, recorded first and made in parts → `dialogue-scenes`. Where the block sits in the
prompt → `video-prompting`. Lines, voices and lip sync → `sound-design`. Joining
passages of several takes → `assembly`. Physical action in the same scene → `action-choreography`.
