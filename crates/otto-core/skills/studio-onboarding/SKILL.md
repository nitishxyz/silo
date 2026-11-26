---
name: studio-onboarding
description: The first conversation in a new Studio. Load when the studio was just created ("Start with the agent"), when studio_read shows no studio.toml, or when world/overview.md is still empty. Interviews the user on premise, genre, tone, audience, references, format and episode plan, then writes overview.md, style.md and studio.toml and makes the first style frame.
---

# Studio onboarding

Turn a blank studio into a project with a premise, a look and a format, in one short conversation, ending with a
first image the user can react to. Aim for 3–4 exchanges, not an intake form.

## 0. Check where you are

1. `studio_read` (the `studio` agent has the Studio tools; another agent loads them first with `load_tools
   {names: ["studio_*", "canvas_*"]}`).
2. If there is no `studio.toml` (the user is not in a studio yet), write one with the four presets below and
   create `world/overview.md`, `world/style.md`, `world/characters/`, `world/places/`, `world/props/`
   (`shell` `mkdir -p` for the empty folders). Then `studio_open` so the user sees the block.
3. If `world/overview.md` already has content, this is not a first conversation: read it and go to the next step
   the user wants instead.
4. Setup already asked for the name and default format. Treat them as answers, not questions to repeat.

## 1. Ask (a few questions at a time)

Open with one line on what you'll do ("A few questions, then I'll write the bible and make a first frame."),
then the first round. Each round is 2–3 questions with concrete options; always add "or tell me, or say 'you
decide'". If the user gives a rich pitch up front, skip what it answers.

Round 1, the story:
- **Premise**: one or two sentences. If they have only a vibe, offer three loglines built from it.
- **Genre and tone**: e.g. "cozy mystery", "grounded sci-fi thriller", "absurd sketch comedy", "melancholy
  slice of life". Ask what it should feel like, and for two comparables ("Dark meets The Lighthouse"): they say
  more than adjectives.
- For a series, **what keeps making stories**: the job, place, secret or relationship that puts a new problem
  in front of the lead every episode. Short vertical series want the conflict built into the premise itself (a
  forced marriage, a rival she must work for), since there is no time to set it up.
- **Audience**: kids, teens, adults; general or niche (anime fans, true-crime viewers, a brand's customers).

Round 2, the look:
- **Medium**: live-action photoreal, anime (cel or modern TV), 3D animation (stylised feature or game
  cinematic), stop motion, 2D illustration, mixed.
- **Visual references**: films, shows, artists, games, photographers, or images they attach. Ask what they like
  in each (the colour, the framing, the grain). Attached images: save them with `copy_attachment {attachment_id,
  path: "world/style/ref-<slug>.png"}`.

Round 3, the delivery:
- **Where it will be published**: TikTok/Reels/Shorts (vertical 9:16), YouTube (16:9), Instagram feed (4:5),
  a film festival or web series (16:9 or a cinema preset), internal pitch.
- **Episode length and count**: suggest from the platform. Vertical drama runs 60–120 s an episode, and the
  apps' series run to dozens of episodes, so propose a first block of 5–10 that can grow. A YouTube series runs
  3–12 min × 4–8; a short film is one 3–15 min "episode". Say that longer episodes multiply shots and render time.

"You decide" means decide well and say what you chose in one line each; do not ask again.

## 2. Write the project

Write all three files in one turn, then show the user the World view with `studio_open {world: "overview"}`.

**`studio.toml`**: set `name` and `format`; keep the presets `studio.init` wrote. Add a preset only when asked for
one that is missing (`[formats.cinema] aspect = "2.39:1"`, `resolution = "1080p"`, `fps = 24`). Edit, never
drop unknown keys.

```toml
name = "Harbor Lights"
format = "vertical"

[formats.landscape]
aspect = "16:9"
resolution = "1080p"
fps = 24

[formats.vertical]
aspect = "9:16"
resolution = "1080p"
fps = 30

[formats.square]
aspect = "1:1"
resolution = "1080p"

[formats.portrait]
aspect = "4:5"
resolution = "1080p"
```

**`world/overview.md`**: the `worldbuilding` skill has the full template. At onboarding write at least:

```markdown
# Harbor Lights

**Logline:** A salvage diver in a drowned port town finds her missing father's diving bell, still warm.
**Genre / tone:** Grounded supernatural mystery; quiet, salt-worn, tender, with dread underneath.
**Comparables:** Dark meets The Lighthouse.
**Audience:** Adults 18–40 who like slow-burn mysteries. No gore.
**Format:** Vertical 9:16, a first block of 8 episodes × ~90 s, for TikTok and Reels. Each episode ends on a hook.
**Engine:** Every dive brings up something from the drowned town that someone wants buried.

## Premise
Three to six sentences: who, where, what they want, what stands in the way, what is strange.

## Rules of the world
- Short, generation-relevant rules (what is possible, what is never shown).

## Series
Arc in two or three sentences; episode loglines come with `series-writing`.

## Open questions
- What we haven't decided yet, so it is not invented silently later.
```

**`world/style.md`**: load `style-guide` and write it from the medium and references. It must end up with a
`## Medium line` (for sheets), a `## Style line` (added to every image and video card when it runs, and written
into plate prompts) and a `## Steer away from` list.

## 3. First style frame

Make one image that sells the look, not a character sheet: a mood shot of the world's key place or moment,
built from the premise, in the project format.

1. Load `image-prompting`, then `canvas_create {title: "Moodboard"}` (no episode).
2. Two cards from two calls at once: `generate_image {prompt: "<a specific moment: subject, place, light
   source, action>", canvas: {name: "moodboard"}}`, each a different moment or angle. The run adds the style
   line and the canvas gives the aspect, so the prompt has neither. Use references only if the user gave some
   (`references: ["world/style/ref-….png"]`, and say "Take only the colour palette and texture of image 1, not
   its subject or layout"). Leave the leads out of it: their faces come from `character-design`.
3. Look at both. Tell the user in two lines what each does differently and ask which way to push (warmer or
   colder, more or less grain, closer or wider). Push by editing the style line in `style.md`, then re-run the
   cards (`canvas_run`); one or two rounds at most here.
4. Save the chosen one as the style anchor: `copy_attachment {attachment_id, path:
   "world/style/frame-01.png"}`, and list it in `style.md` under `## Style frames`.

## 4. Propose next

End with a short plan and start on the first item unless the user redirects. On a studio that is already
under way, `studio_read`'s Production block lists the next steps in order: propose those instead.

1. The lead characters (`character-design`): names and one-line concepts for 2–4 of them, pulled from the
   premise. Offer to design the lead first.
2. The key place (`location-design`), and any vehicle or object the story turns on (`prop-design`).
3. The series outline and episode 1 script (`series-writing`), then shots (`shot-listing`).

Any document you write for a canvas (`canvases/<name>/script.md`, `references.md`, a music brief) goes on that
canvas as a document card at the top-left (`add_node` of an asset whose `path` is the `.md` file), not restated
in a note; tell the user where it is ("the script is pinned on the canvas, top left").

## Pitfalls

- Too many questions: never more than three in a message, never a numbered form of ten.
- Asking what Setup already set (name, format) or what the pitch already said.
- Vague tone words in `overview.md` ("epic", "cinematic", "high quality"): replace them with concrete ones.
- Making a character before the style exists: the sheet would follow a style you then change.
- Pasting the style line into the style frame's prompt, or turning the card's style off: the run adds it once,
  and the style line is what you are testing.
- Leaving "you decide" answers out of `overview.md`: every decision is written down, including yours.
