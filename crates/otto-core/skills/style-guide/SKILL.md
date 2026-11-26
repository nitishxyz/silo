---
name: style-guide
description: Writing or changing a Studio's visual language in world/style.md - medium, palette, lenses and framing, lighting, texture, era, what to steer away from - so it works as prompt text, with a medium line for sheets, a style line that every image and video card gets when it runs and every plate carries, and an optional motion line for video cards; then a set of style frames in world/style/. Load during onboarding once the medium and references are known, when the user wants to change the look, or when generations stop looking like one show.
---

# Style guide

`world/style.md` is read by people in the World view, by every card run (its style line) and by you when you
write prompts. Its job is to make a
hundred generations by different models look like one show. Write every line so it could go into a prompt as
is. Load `image-prompting` for general prompt technique; this skill is about the show's look.

## 1. Gather

From onboarding or the user: the medium, the references (titles, artists, attached images) and what they like
in each, the tone from `world/overview.md`, the format (vertical changes framing). Save attached references to
`world/style/ref-<slug>.png` (`copy_attachment`); the World view shows `world/style/` as a strip.

Translate names into visible properties: a title alone leaves the model to guess which of its qualities you
mean, and the guess changes from prompt to prompt. "Like Severance" becomes "symmetrical frontal framing, sterile off-white offices,
low-saturation greens, overhead fluorescent light". Write the properties; keep the title only as a note.

## 2. `world/style.md` template

```markdown
# Style

## Medium line
Photorealistic, shot on 35 mm film, natural skin texture.

## Style line
Photorealistic 35 mm film still, muted sea-green and rust palette with warm-white practical lights,
soft overcast daylight, 35 mm lens at eye level, fine film grain, quiet and salt-worn.

## Medium
Live-action photoreal, grounded, contemporary but worn. Real materials, nothing glossy.

## Palette
- Dominant: sea green (#5E7F73), wet slate grey (#4A5257).
- Accents: rust orange (#B5562C) for the bell and the Guild's lamps; Mara's yellow boots (#E1B12C) are the
  only saturated yellow in the show.
- Skin and practical lights: warm white. Night: deep blue with sodium-orange pools.

## Camera and framing
- Lenses: 35 mm for most shots, 85 mm for close-ups; wide shots at 24 mm only to establish.
- Eye-level camera; low angles only for the Guild.
- Vertical 9:16: one subject per frame, faces in the upper third, headroom tight; wides are tall, not broad.

## Lighting
- Day: soft overcast from the sea side. Interiors: window light plus oil lamps, warm white.
- Night: sodium street lamps, single sources, deep shadow between them.

## Texture
Fine film grain, soft highlight roll-off, wet surfaces, peeling paint, salt stains.

## Era and design
Present day after a flood forty years ago: 1980s objects patched and reused; no new technology on screen
besides old phones and radios.

## Steer away from
- Glossy, high-saturation "AI" look → matte, weathered surfaces, muted palette.
- Teal-and-orange blockbuster grade → sea green and rust only.
- Clean modern interiors → patched, salvaged, hand-repaired.

## Style frames
- world/style/frame-01.png: the quay at dawn (approved, the anchor).

## Reference notes
- Dark (2017): colour discipline, rain, quiet dread.
```

How each part is used:
- **Medium line** (5–15 words): the rendering only. It goes on character, prop and faction sheets, which must
  stay neutral (white background, no mood light, no grade; see `character-design`).
- **Style line** (20–40 words): medium, palette, light, lens, texture, mood. Keep it under the `## Style line`
  heading, as one paragraph: a run appends that text, unchanged, to every image and video card (keyframes,
  shots, style frames) unless the card's style is off. You write it yourself only at the end of prompts for
  files made with `path` (plates, place sheets, posters). Change it only here, never ad hoc in a prompt.
- **Motion line** (optional, `## Motion line`, 25–45 words): how things move, for shows whose medium is not
  live action. A run appends it after the style line to every video card, never to images. Without the
  heading nothing is added. `animation-direction` writes it; add it as soon as an animated or stylised show
  makes its first video.
- **The sections below** are the long form you consult when writing a specific prompt (which lens for a
  close-up, what night looks like); pull the relevant line in, don't paste the whole file.

## 3. Writing rules

- **Name the medium plainly and first.** "Photorealistic" engages photoreal rendering; for others be exact:
  "2D anime, cel shading, clean line art, flat colour with one shadow tone", "3D animated feature, soft
  subsurface skin, stylised proportions", "stop-motion, felt and clay, visible fingerprints, miniature set".
- **Describe light by its source, direction and colour**, not by its effect: "a single sodium lamp from frame
  right", not "her face glows orange". Use "warm white" and "cool white" for colour temperature; a saturated
  colour word ("yellow light") gives you a yellow cast.
- **Palette as named colours**, with hex codes in the file for people and for FLUX, which takes them. Keep
  2–3 dominant colours and 1–2 accents tied to story meaning.
- **Lens and angle words for look and composition**, not for exact optics: models read camera specs loosely.
- **A lens look is written as what it does to the picture**, not as the lens's name. For shallow focus: the
  one thing that is sharp, how near the camera is to it, and which layers dissolve ("focus on the nearest
  mooring post, camera a metre from it, the boats behind melting into soft blur"). For out-of-focus lights:
  their shape and what they sit on ("tall oval highlights on the wet stones"). For a flare: its colour, its
  direction and the lamp it comes off; without a bright source in frame there is nothing to flare. Wide lenses
  stretch faces: keep close-ups on the longer lens. In video prompts a field of view in words ("a wide view
  taking in the whole quay") holds better than millimetres. No brand names of cameras, lenses or film stocks:
  describe the look, or a logo may turn up in the frame.
- **Texture that is physical**: grain, roll-off, materials. Words like "dust", "scratches" or "chromatic
  aberration" get drawn as defects on the image.
- **Leave out empty boosters**: "cinematic", "masterpiece", "8K", "award-winning" add nothing you can see.
- **Steer away from, written positively**: models follow what is named, so an avoid list of nouns invites
  them. Pair each thing to avoid with what to do instead, and put only the "instead" in prompts.
- **Vertical is its own grammar**: if the format is 9:16, say how framing changes (one subject, faces high,
  tall compositions). `shot-listing` applies it per shot.

## 4. Style frames

Style frames test the style line before characters and shots depend on it. Make 3–4 on the Moodboard canvas
(`canvas_create {title: "Moodboard"}` if it doesn't exist), each a different situation the show will need:

1. The key place in its default light (the anchor).
2. A night or interior scene.
3. A close-up of a face (an unnamed person who is not a lead, so it doesn't fix a character's look).
4. An action or weather moment, if the show has them.

```
generate_image {prompt: "<one specific moment: subject, place, action, light source>",
                canvas: {name: "moodboard"}}
```

One call per frame, all at once; each makes a card. The run adds the style line and the canvas gives the
aspect, so the prompt carries neither. With user references, pass them and say what to take: "Take only the
colour palette and film texture of image 1; the scene is <…>."

Review them side by side: do they read as one show? Adjust the style line (not each prompt) and re-run the cards
with `canvas_run`; each run adds a take, so the earlier look stays to compare. Save approved frames with `copy_attachment {attachment_id, path: "world/style/frame-0N.png"}` and
list them under `## Style frames`. `frame-01.png` is the anchor other skills pass as a style reference.

## 5. Changing the look later

1. Edit `style.md` (medium line, style line, sections) and remake the style frames.
2. Sheets made with an old medium line no longer match: regenerate them (`character-design`,
   `location-design`), keeping the old ones in `refs/archive/`.
3. Cards wired from remade sheets show Out of date. A style line change marks nothing (it is not a wire), yet it
   reaches every image and video card's next run: tell the user which shots that is and offer to re-run them
   (`continuity`).

A palette or grade tweak that leaves the medium alone needs no new sheets.

## Pitfalls

- A style line that changes from prompt to prompt: models drift with every variation.
- Putting mood and grade on sheets: every shot inherits it and fights the scene's own light.
- Copying a reference's content along with its style: always say which properties to take.
- Style frames of the leads: they fix a face before `character-design` has made the sheet.

## Next

Characters → `character-design`. Places → `location-design`. Per-shot framing in the format → `shot-listing`.
How an animated or stylised show moves (the motion line) → `animation-direction`.
