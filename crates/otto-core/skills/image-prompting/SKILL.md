---
name: image-prompting
description: Writing prompts for stills - image cards on a canvas (keyframes, shot plates, style frames) and direct generate_image calls (character/place/prop sheets, plates, posters, thumbnails) - and fixing a still that came out wrong, with a better prompt or an edit. Covers prompt structure, references and their roles, edits and markup, model choice, and what each image model family responds to. Load before generating any still in a Studio, and again when a result drifts from its sheet, garbles text, ignores the style or doesn't fit the shot.
---

# Image prompting

A still is either an end product (poster, thumbnail, sheet) or the first frame of a video. Write every prompt so
a stranger could draw it: visible things, where they are, how they are lit. Mood words alone ("epic",
"beautiful", "premium") give the model nothing to draw; name the light, material and framing that make the mood.

## 1. Prompt structure

Order, most important first (FLUX and Recraft weight earlier words more; every vendor names the same parts):

1. **Job**: what the image is. "Keyframe, first frame of a shot", "character sheet", "poster", "product photo".
2. **Subject**: who or what, with the two or three details that must survive (scar, coat, colour of boots).
3. **Action / pose**: body framing, gaze, what the hands do ("both hands on the coiled rope at her hip,
   looking past the camera to frame right"). Say "full body visible, feet included" when you need it.
4. **Setting**: the place, with foreground, middle ground and background when depth matters.
5. **Composition and shot size**: extreme wide, wide, medium, close-up, insert; where the subject sits in frame;
   camera height and angle (eye level, low angle, top-down).
6. **Lens**: 24 mm wide with straight verticals, 35 mm, 50 mm natural, 85 mm portrait, 135 mm compressed;
   shallow or deep depth of field. Lenses are appearance cues, not physics (OpenAI's guide says so).
7. **Light**: the source, its direction, time of day ("low sun from frame left, long shadows"). Lighting changes
   results more than anything else in BFL's guide; "soft even light" flattens faces.
8. **Palette and medium**: hex colours with their names (`sea green #5E7F73`) work on FLUX, Ideogram, Recraft;
   film stock or format as a look ("35 mm film, fine grain", "Kodak Portra 400 colour").
9. **What must not change** (edits and referenced subjects): a separate sentence starting "Keep …".

- **Sentences, not tag lists.** Every model here reads prose best; tags lose who holds what and what sits behind
  what. GPT Image also takes labelled sections (Scene / Subject / Details / Constraints) for complex frames.
- **Length.** BFL puts FLUX's sweet spot at 30-80 words; GPT Image, Nano Banana, Seedream and Recraft follow long
  prompts, but each clause must name something visible. Over-specified prompts fight themselves; cut adjectives.
- **Say what you want, not what you don't.** FLUX has no negative prompt; Gemini and Ideogram ask for "semantic
  negatives" ("an empty, deserted quay", not "no people"). A short exclusion line is fine on GPT Image
  ("No extra text, no watermark").
- **Text to render**: the exact words in double quotes, early in the prompt, with position, size, weight, colour,
  and reading order for several blocks (top to bottom). Short phrases; spell unusual names letter by letter.
  Describe lettering ("heavy condensed sans-serif, all caps") rather than naming a font. Long copy fails on every
  model: put it on in assembly instead.

## 2. References

In a direct call, `references` takes project paths or `att_` ids; the first is image 1, and so on. On a card,
mention each one in the prompt instead: `@characters/mara` (an entity's sheet), `@characters/mara#hood-down` (one
of its named references, by its file's name without the extension: `refs/hood-down.png`) or `@#n_3f2a9c` (a node: another card, a file on the canvas) wires it into `reference`
when the prompt is written, and the run writes it out as "Mara Venn (image 2)" with its number as actually sent,
and a named reference with its job: "Mara Venn, Hood down (image 3: take only her hair once the hood is down)".
So a card's numbering is never wrong. Hand numbering ("image 3 defines only…") is for direct calls.

- **Give each reference a role in the prompt**, by number (on a card, by mention: "@characters/mara is her
  identity: …"): "Image 1 is Mara's identity: keep her face, scar and
  coat. Image 2 is the harbor: keep the quay and the bell tower. Image 3 is a style reference only: take its
  palette and grain, not its content." An unassigned reference gets blended or copied whole.
- **Subject references** (sheets, turnarounds, a product) carry identity; **style references** (style frames)
  carry palette, texture, medium. Never let one image do both jobs silently.
- **How many**: only what the frame needs, sheet first. Limits: Nano Banana 2.1 holds up to 4 characters and 10
  objects (14 images); Nano Banana Pro 5 characters, 6 objects, 3 style images; Grok Imagine 5 on xAI; Seedream
  5.0 Pro edit 10; FLUX.2 pro 8; Ideogram one; Soul, Recraft none. Check `media_model_schema` for the rest.
- **Repeat the anchors in words** even with the sheet attached: Gemini's guide says to describe critical details
  (face, logo) in detail alongside the edit; Seedream's says three named anchors (a feature, a material, a light
  setup) repeated verbatim keep a face across a series.

## 3. Edits versus new generations

Edit (the image as image 1) when most of the frame is right; generate anew when the composition is wrong.

- **Describe the change, not the scene.** Re-describing the whole image invites the model to regenerate it.
- **One change per call, then a keep list in its own sentence**: "Change only X. Keep A, B, C, the light and the
  framing unchanged." Repeat the keep list every round; chain several edits rather than one big one.
- **Relight end to end**: "from flat midday to warm dusk with the windows lit", not "warmer".
- **Restyle with a concrete medium** ("loose oil painting, palette-knife texture"), letting the source hold the
  composition.
- **`markup`** (GPT Image 2.5/2.6): a transparent PNG the size of image 1 with circles or arrows where the
  change goes, usually drawn by the user. It applies to image 1 only; other references stay references. Name
  the mark in the prompt ("inside the red circle") and end with "Remove the red circle from the finished image."
- Edits don't need the style line (the source carries the look): leave it out of a direct edit call. An edit
  card gets it from its run, which is harmless; for a restyle away from the show's look, turn the card's style
  off (`options.style: false`).
- When part of an image must stay pixel-identical, OpenAI's advice is to composite the edit back over the
  original, not to hope.

## 4. Settings

- **`n`**: 2-4 when exploring in a direct call (sheets, a new look, options to show the user); 1 once the
  prompt is proven. On a canvas, alternatives are takes of one card (run it again) or cards side by side. Pick
  one and say why; a shot's keyframe card shows one picked take.
- **`quality`**: low or medium for drafts; high for final keyframes, sheets and anything with small text. On
  providers without a quality setting, high means their larger size (Gemini 2K, xAI 2k, Soul 1080p).
- **`complexity`** (GPT Image 2.6 only; default high): how much it plans the layout. It matters when the prompt
  leaves design decisions open (posters, thumbnails); it changes little on a fully specified frame. Compare
  complexity at a fixed quality, and quality at a fixed complexity.
- **`transparent`**: cutouts, props for compositing, logos. OpenAI and Seedream 5.5 can; Gemini can't. Also
  write "transparent background, clean alpha edges, no backdrop" in the prompt, then check the alpha.
- **`format: "svg"`** (GPT Image 2.6, `n: 1`, no references) for a real vector file; `gif` for a short loop.

## 5. Choosing a model

Ids move fast: run `list_media_models {kind: "image"}` and trust it over this list; `media_model_schema` before
any `generate_media` call. Respect a choice the user made or one saved in `studio.toml` `[models]`.

| Job | Reach for |
|---|---|
| Keyframes with several entity sheets | Nano Banana 2.1 / Pro (most references), GPT Image 2.6 Sunburst, Seedream 5.x edit, FLUX 3 edit |
| Precise local edits | GPT Image 2.6 with `markup`, Grok Imagine 2.0, Seedream (holds unedited areas across rounds) |
| Photoreal editorial look, no references | Soul 2, Soul Cinema (cinema stills), Krea 3 |
| Text: posters, thumbnails, title cards | Ideogram 4.x, GPT Image 2.6, Nano Banana Pro, Seedream 5.x |
| Design, flat illustration, brand palettes | Recraft V4.1 (raster here; `colors`/`background_color` via `generate_media`) |
| A vector file | GPT Image 2.6 with `format: "svg"` |
| Fast drafts | gpt-image-2.6-flare, nano-banana-2-lite, flux.3-klein, z-image-turbo |

Say in one line why you picked it. Higgsfield's Soul takes no references: a call with references there goes to
Grok Imagine, so don't pick Soul for a shot that needs an entity's sheet.

## 6. In a Studio

- **Card or file decides the style line and size.** A prompt written into a card (`canvas_edit`, or a
  `generate_image` call with `canvas`) leaves out the style line and `aspect_ratio`: the run appends the
  `## Style line` of `world/style.md` and sizes from the effective format. A file made with `path` (plates,
  place sheets, posters, thumbnails) ends with the style line verbatim and sets its aspect; character, prop
  and faction sheets take the `## Medium line` only (`character-design`). Never paraphrase either; if it's
  wrong, fix `style.md`.
- **Entities.** Anyone or anywhere that exists in `world/` goes in as its sheet (and the named references that
  fit the angle: the profile turnaround for a profile, the state or outfit worn, the place's view for that
  angle), with its role named in the prompt. On a card: its mention, `@characters/mara` (wires its Entity node's
  sheet) or `@characters/mara#profile` (wires that reference and states its `defines`). Pull
  two or three anchors from the entity's `.md` summary into the prompt in the same words every time.
- **Keyframes are cards.** In each shot frame (from `canvas_read`): an image card titled `S2.3 key`, its prompt
  from the shot note, the entities mentioned in it (`@characters/mara`: that wires them), then `canvas_run` all of a scene's keyframes at once
  (example below). A note wired into `text` is added as context; don't wire the shot note itself, its
  bookkeeping lines would land in the prompt.
- **Match the shot list.** Read the shot's note (`canvas_read`): shot size, angle, lens, who is where, screen
  direction. The keyframe uses that framing, not the model's default centred medium shot. See `shot-listing`.
- **Keyframes for video** (the next step is `video-prompting`): the image becomes the video's first frame and
  fixes its composition, subject, light and style, so:
  - Compose for the motion that follows: leave room in the direction of travel or the camera move.
  - Show the first frame of the action, not its middle: weight on the back foot before the throw, hand on the
    door before it opens. Mid-action poses, motion blur, dust trails and speed lines are motion cues the video
    model follows; prompting against them fails (Runway's guide).
  - Sharp, still, no motion blur; a clear readable pose; nothing cropped that must move into frame.
  - Faces and hands large enough to hold detail: blurry faces and hands get worse once animated. Prefer a
    medium shot over a wide one for anything with dialogue or expression.
  - Within a scene, the next shot's first frame is often the last frame of the one before (`continuity`).
- **Look before you use it.** The result arrives as a 768 px preview; `read` the attachment for detail. Check it
  against the sheet (face anchors, costume, props, palette), the shot note (framing, screen direction), the
  text letter by letter, and for keyframes the motion cues above. Say what's wrong and fix it before animating:
  a better prompt or wires and a re-run of the card (a new take), or an edit; a flawed keyframe costs a video
  generation.

## 7. Model families

**GPT Image 2.x (OpenAI).** Any format works: short prose, paragraphs, labelled sections, JSON-like. Ask for
"photorealistic" or "real photograph" explicitly. People: body framing, scale, gaze, hand contact. Text: quotes,
position, typography, how many times it appears, "no extra text". Edits: "change only X", the preserve list,
and exclusions. Number references by role. Iterate by passing the last output back with one change and the
constraints restated. Flare is fast; Sunburst is for detail and precise edits. `markup`, `complexity`, svg/gif.

**Nano Banana (Gemini).** Be hyper-specific ("ornate elven plate armour etched with silver leaf", not "fantasy
armour"). State the purpose ("a keyframe for a quiet drama"); it shapes the result. For busy frames, write
step by step: background first, then the foreground, then the hero element. Semantic negatives. Camera terms
(wide-angle, macro, low-angle). Many references with clear roles; describe preserved details in words too. Decide
exact text first, then ask for the image with it. Each call stands alone here: restate everything.

**Grok Imagine 2.0 (xAI, also via Higgsfield and fal).** Name five layers: subject, setting, composition, light,
finish; the finish clause ("editorial photograph", "flat illustration") sets the register, and leaving it out
hands the look to the default. Reads photographic terms directly. The aspect ratio recomposes the frame
rather than cropping it. Name the details that must be right in busy scenes. Edits: describe the change, name
what stays; output follows image 1's aspect unless set. Feed a clean, sharp source.

**FLUX (fal; BFL's guide is for FLUX.2, apply it to FLUX 3 until BFL publishes one).** Subject, action, style,
context, lighting, technical (camera, lens, film stock), front-loaded. No negative prompts. Hex colours. Quoted
text. Editing and references go to the edit endpoint: "the person from image 1, wearing the jacket from image 2,
on the street from image 3". Break big edits into a chain of single changes.

**Seedream 5.x (Higgsfield, fal).** Five parts: image type first, subject with details that must stay, scene
and light (source, direction, time), camera (lens, framing, distance, depth of field, grain) in short technical
words, and change/keep in separate sentences. Quoted text with position, size, weight, colour. Name each
reference's job ("Reference 1 provides the face"). "Visible pores, soft shadow, no gloss" instead of "perfect
skin". 5.5 makes native transparent PNGs; start the prompt with "transparent background".

**Soul 2 / Soul Cinema (Higgsfield).** Text only; fashion, editorial and cinema-still looks. Write it like a
photo brief: age, hair, skin texture, exact clothing, background, light, camera ("shot on medium format").
Good for style frames and lookdev; not for shots of existing entities.

**Ideogram 4.x.** The text model: quoted words near the start, where they sit, kept short, in a simple scene.
Several blocks in reading order, each with words, style and place. Weights and flags (`::2`, `--ar`) are read as
words; set size through the call. English renders most reliably. One reference at most.

**Recraft V4.1.** Short prompts make it interpret (good for exploring); structured prompts make it execute,
global to local. For flat/vector-look work define graphic type, shape logic, a strict palette, line discipline,
layout and constraints ("flat colours, no gradients, no shadows"); skip texture words. Text: quoted, with the
typographic hierarchy. No stacked evaluative adjectives.

## 8. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Wrong face, drift across shots | Sheet missing or unassigned; face too small; anchors not in words | Sheet as image 1 with its role; the entity's anchors verbatim; frame closer; or edit: "make the face match image 2, change nothing else" |
| Extra or fused fingers | Hands small, ambiguous action | Describe the grip ("right hand wrapped around the rope"); show hands clearly or keep them out; fix with a `markup` edit on the hand |
| Garbled text | Unquoted, long, late in the prompt, tiny | Quote it early with position and size; shorten; quality high; Ideogram or GPT Image; or render text-free and add text in assembly |
| Style ignored | Card's style off; style line missing or paraphrased in a direct call; a photo reference pulling away; mood words | Card style on, or the style line verbatim in a file prompt; add the style frame as "style only" reference; replace mood words with light, palette, medium |
| Reference copied too literally | No role given; sheet's pose, background or light leaking in | Say what to take ("only her face and coat from image 1; new pose, new setting"); fewer references; neutral sheets (`character-design`) |
| Composition wrong for the format | `aspect_ratio` set against the canvas; placement not described | Let the canvas set format; say where the subject sits for that format; regenerate at the format, don't crop |

## 9. Worked examples

Keyframe card (shot 3 of the pilot, medium shot, Mara about to throw a line). The shot frame is `n_sh03` (from
`canvas_read`); the mentions wire Mara's and the harbor's sheets (their Entity nodes, added if the scene has none):

```
canvas_edit {canvas: "ep01-pilot", ops: [{op: "add_node", node: {parent: "n_sh03", body: {kind: "gen",
  makes: "image", title: "S1.3 key",
  prompt: "Keyframe, first frame of the shot. Medium shot at eye level, 35 mm: @characters/mara stands at the
    edge of the stone quay of @places/harbor, weight on her back foot, both hands on a coiled rope at her hip,
    about to throw it toward a boat out of frame right. She faces three-quarters to frame right; open water
    fills the right third of the frame. Sharp and still, face in focus. @characters/mara, her sheet, is her
    identity: keep her face, the scar through her left eyebrow, her father's oversized brown oilskin coat and
    her yellow boots. @places/harbor, its sheet: keep the bollards and the bell tower behind her."}}}]}
→ ids: ["n_k3"], mentions: both wired reference
canvas_run {canvas: "ep01-pilot", nodes: ["n_k3", …the scene's other keyframe cards]}
```

The run sends "Mara Venn (image 1) stands at the edge of the stone quay of The Harbor (image 2)…". No style line
and no aspect: the run adds them. Quality and other settings go in the card's `options` (names per
the tool's description).

Edit (the user circled the coat on that keyframe), one direct call; the result is a new card in the shot frame:

```
generate_image {
  prompt: "Change only the coat inside the red circle: soaked dark with rain, water dripping from the hem.
    Keep her face, expression, pose, the rope, the quay, the light and the framing unchanged. Remove the red
    circle from the finished image.",
  references: ["<the keyframe card's picked take>"],
  markup: "canvases/ep01-pilot/assets/s01-03-markup.png",
  provider: "openai", model: "gpt-image-2.6-sunburst",
  canvas: {name: "ep01-pilot", frame: "n_sh03"}
}
```

Then wire the new card into the shot's video card as `first_frame` (it replaces the old wire).

Multi-reference card prompt (two characters, a place, a style frame on the canvas as asset node `n_sf1`); each
mention wires its source into `reference`:

```
Keyframe, first frame of the shot. Medium two-shot, 35 mm, eye level: @characters/mara on the left and
@characters/tobin on the right face each other across a rusted salvage crate on the deck of @props/tug. Mara's
hand rests flat on the crate lid; Tobin leans back, arms crossed. @characters/mara, her sheet: her face, brow
scar, brown oilskin coat. @characters/tobin, his sheet: his face, red beard, oilskin jacket. @props/tug, its
sheet: the deck, the wheelhouse behind them, the winch at frame left. @#n_sf1, the style frame, is a style
reference only: take its palette, grain and soft light, none of its content.
```

The same prompt in a direct `generate_image` call (a poster, a file with `path`) numbers the references by hand
("Mara Venn (image 1)"), ends with the style line and sets `aspect_ratio`.
