---
name: worldbuilding
description: Writing and maintaining a Studio's world in world/: overview.md (premise, rules, history, timeline, series arc), custom sections such as factions, vehicles or creatures, and what belongs in overview.md versus an entity folder. Load when building out the world after onboarding, when adding a new kind of thing to the world, or when the story has invented facts that need writing down.
---

# Worldbuilding

`world/` is the bible every generation is checked against. Keep it short, concrete and written for two readers:
the user browsing the World view, and you pasting lines into prompts. A fact that is not in `world/` does not
exist; a fact in two places will drift.

## What goes where

| Kind of fact | Where |
|---|---|
| Premise, genre, tone, audience, format, rules of the world, history, timeline, series arc and episode loglines | `world/overview.md` |
| The visual language, the style line, what to avoid | `world/style.md` (`style-guide`) |
| A person, creature or anything with a face and a voice | `world/characters/<id>/` (`character-design`) |
| A place the camera goes | `world/places/<id>/` (`location-design`) |
| An object that recurs on screen and must look the same (the diving bell, a ring, a car) | `world/props/<id>/` |
| A group, vehicle class, species, organisation that recurs | a custom section: `world/factions/<id>/`, `world/vehicles/<id>/`, `world/creatures/<id>/` |

Rules of thumb:
- **Make an entity when it will be generated more than once** or must look the same twice. A tavern mentioned
  once is a line in `overview.md`; the tavern where three scenes happen is `places/tavern/`.
- **Facts about one entity live in its file**; `overview.md` links to it by id (`characters/mara`) instead of
  repeating its description.
- **Ids are lowercase slugs**, short and stable: `mara`, `old-bell`, `harbor-guild`. The folder and its `.md`
  share the id (`world/factions/harbor-guild/harbor-guild.md`). Don't rename ids once shots reference them.

## `overview.md` template

Keep it under ~150 lines. Sections in this order (onboarding writes the first ones):

```markdown
# <Title>

**Logline:** one sentence.
**Genre / tone:** concrete words.
**Comparables:** two titles ("X meets Y").
**Audience:** who, and what is off limits.
**Format:** aspect, episode length × count, platform.
**Engine:** the job, place, secret or relationship that hands the lead a new problem every episode.

## Premise
## Rules of the world
## History and timeline
## Factions and powers        (one line each, linking to world/factions/<id>)
## Places                     (one line each, linking to world/places/<id>)
## Series                     (arc and episode loglines: series-writing)
## Open questions
```

**Rules of the world** are the constraints that change what a shot may show. Write them as testable statements:

```markdown
- The sea rose forty years ago; the old town is underwater up to the second floor. Streets are canals.
- Electricity is rationed: lights come on only from 18:00 to 22:00. Daytime interiors are lit by windows and
  oil lamps.
- The drowned bells ring only for someone who is about to disappear. Nobody else hears them.
- No guns. Harbor Guild wardens carry gaff hooks and whistles.
```

Not: "Magic is mysterious and has a cost." That cannot guide a single frame.

**History and timeline**: dated bullet points, oldest first, with "now" marked. Use the story's own calendar if
it has one. Add each episode's events as they are written (`series-writing` keeps this in step).

```markdown
- Year 0: The Rise. The old town floods in one winter.
- Year 12: The Harbor Guild takes over salvage rights.
- Year 37: Mara's father, Ilan Venn, disappears on a dive at the Old Bell.
- Year 40 (now, ep 1): Mara finds his diving bell, still warm.
```

## Custom sections

Make the folder and the first entity in one go; the World view shows the folder name as the section title.

`world/factions/harbor-guild/harbor-guild.md`:

```markdown
---
name: Harbor Guild
sheet: sheet.png
tags: [faction, antagonist]
summary: Salvage cartel in oilskin coats with brass whistles; controls every dive permit in the port.
---
## Look
Dark green oilskin coats, brass whistle on a chain, gaff hook, numbered armband (white on green). Clean-shaven
or close beards. Move in pairs.

## What they want
Everything that comes up from the old town passes through their scales; they tax a third.

## Members
- characters/warden-oske (lieutenant)
```

A faction's or prop's `sheet.png` is generated like a character's (see `character-design`: plain white
background, soft directional key light, the medium line only, `image-prompting` for the prompt): for a faction,
the uniform on one member, full body; for a prop, the object alone, three-quarter view, filling the frame. Add
more angles as `refs/` when shots need them. A custom-section entity takes the same frontmatter keys as any other;
unknown keys are kept, so add what helps (`era`, `size`, `colors`).

## Writing for generation

- **Lead with what the camera sees.** Colours, materials, shapes, sizes, wear. "Brass, green with verdigris,
  head-sized porthole, barnacles on the lower third" beats "an ancient relic".
- **One `summary` line per entity**, under ~25 words, that could be pasted into a prompt as is.
- **Short beats long.** Backstory only as far as it changes behaviour or looks. A page of lore nobody
  generates from is noise in your context.
- **Name things once, the same way.** If it is the "Old Bell", it is never "the great bell" in another file,
  a script's scene heading or a prompt. One spelling per name is what references and continuity hang on.
- **Mark the unknown** under `## Open questions` instead of inventing it in a prompt.

## Keeping it in step

The story will invent things while you write scripts and shots. In the same turn that it does:

1. A new fact about an entity (a scar, a new coat, a broken arm in ep 3): `edit` its file. If it changes the
   look from some point in the story on, add a dated line rather than overwrite (`From ep 3: left arm in a
   sling.`) and say which outfit or ref covers it (`character-design` makes the new outfit sheet).
2. A new recurring place, object or group: create the entity (file first; its sheet when it is first shown).
3. A new rule or event: add it to `## Rules of the world` or `## History and timeline`.
4. A contradiction with what exists: say so to the user in one line, propose which version wins, and fix the
   files once decided. Never leave both.
5. A change to an entity's sheet or refs: the cards wired from them show Out of date on their canvases. A change
   to its file alone marks nothing, so find the shots whose notes name it. Tell the user which, and offer to
   re-run them (`continuity`).

## Pitfalls

- Generic fantasy filler ("ancient prophecy", "dark forces"): every line should be specific to this world.
- Copying a character's description into `overview.md`: link the id instead.
- Entities without `summary`: the World view card is empty and you lose the one-line prompt fragment.
- Sections nobody uses: don't make `vehicles/` for one boat seen once.
- Recording world facts in canvas notes or card prompts: the canvas is the board, not the bible.

## Next

New characters → `character-design`. New places → `location-design`. The look → `style-guide`. The story's
arc and episodes → `series-writing`.
