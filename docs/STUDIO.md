# Studio block

Status: S1 to S3 built (server, tools, otto, the `studio` agent and its skills, the pane, the board) and covered
by tests; the app's pane and board compile and have unit tests but have not been run by hand yet. S4 (assembly
beyond the `assembly` skill's ffmpeg, a timeline) is not started. Where the built code differs from the text, the
**Built** paragraphs (§5, §6.2) say how.

A block for making content with an agent: images, video, audio, and eventually whole series. A studio is a
project: a folder holding a **world** (the bible: characters, places, props, style, lore) and any number of
**canvases** (boards where media is generated, arranged and linked; an episode is a canvas). A built-in `studio`
agent with curated skills works in it as a co-creator, alongside the user or on its own.

It builds on what media generation already does (docs/MEDIA.md: `generate_*`, `generate_media`, background jobs,
the agent block's image/video/audio tiles and viewers). What it adds is a place where the work lives and stays
organised, and a world the generations stay consistent with.

Decided on 2026-11-24:
- **A block kind of its own**, `BlockKind::Studio`, on the server like Files and Changes: it works on remote
  machines and survives restarts; the app only draws it.
- **The data is plain files in the folder.** Open Silo in an empty folder, open a Studio, and start. No database,
  no git requirement; versioning, sharing and what to keep are the user's call.
- **Canvases are tabs** in the block's header. An episode is a canvas with an episode number; there is no
  separate episode type, so the user or the agent makes one whenever it's needed.
- **The world is not a tab:** an icon on the right of the header opens the World view, with a switcher for its
  sections (Overview, Style, Characters, Places, Props, …).
- **Formats:** the project has a default (16:9, vertical, square, …) that can be switched, and each canvas (and
  each shot) can override it. Generations aimed at a canvas inherit the effective format.
- **Agent:** a built-in `studio` agent next to `build`, with bundled skills for every stage (onboarding, world
  building, character sheets, scripts, shot lists, per-model prompting, continuity, sound, assembly).
- **Cards and wires** (2026-11-24, after ved-web's canvas): the canvas is a node graph you generate from
  directly. A **card** has its prompt and a Run button on it, input ports on its left and an output on its right;
  what is wired into a card is what it is made from. The agent works the same cards with tools; nothing on the
  canvas needs the agent to run.
- No budgets or cost gates for now.

---

## 1. On disk

```
my-show/                         # the block's root
  studio.toml                    # the project: name, formats, canvas order
  world/
    overview.md                  # premise, genre, tone, audience, rules of the world, lore, timeline
    style.md                     # visual language: palette, lenses, lighting, texture, reference notes
    characters/
      mara/
        mara.md                  # frontmatter + description, backstory, mannerisms
        sheet.png                # the canonical sheet (named in the frontmatter)
        refs/                    # one image each: angles, expressions, outfits, anything else
    places/
      harbor/  harbor.md, sheet.png, refs/
    props/ …
    <any folder>/ …              # a custom section (factions/, vehicles/, creatures/)
  canvases/
    ep01-pilot/
      canvas.json                # nodes, edges, format override, episode number
      script.md                  # optional; the agent writes it when the canvas is an episode
      references.md …            # other documents (*.md, *.markdown, *.txt): listed under Documents (§4.2)
      assets/                    # everything generated or dropped on this canvas
      out/                       # assembled cuts
    moodboard/ …
```

- A **section** is a folder under `world/`. `characters`, `places` and `props` are made by `studio.init`; any other
  folder is a custom section, shown with its folder name. `overview.md` and `style.md` are the Overview and Style
  sections.
- An **entity** is a folder in a section with a markdown file of the same name (else the first `.md` in it):

  ```markdown
  ---
  name: Mara Venn
  sheet: sheet.png            # canonical reference, relative to the entity folder
  refs:                       # optional; default: every image in refs/
    - refs/face.png           # a path: a reference named by its file ("face")
    - file: refs/hood-down.png
      name: Hood down
      defines: her hair once the hood is down; not her face   # what to take from it; the rest stays behind
  links: [props/compass]      # entities that go with it (a prop and its owner); read from both ends
  voice: elevenlabs/JBFqnCBsd6RMkjVDRZzb               # characters: a generate_audio voice
  tags: [lead, crew]
  summary: Salvage diver, 30s, scar across the left brow, never takes off her father's coat.
  ---
  Backstory, personality, how she moves and talks…
  ```

  The id is its path in `world/` (`characters/mara`). Only `name` is required; the rest helps the agent and the
  World view. Unknown keys are kept as they are.
- **Named references.** Each `refs` entry is a path or a table `{file, name?, defines?}` (block style as above,
  or flow `{file: refs/a.png, name: A}`); other keys in a table are kept. A reference's **id** is the slug of its
  file's stem (lowercase ASCII letters and digits, one `-` between words, at most 48: `refs/crown-off.png` →
  `crown-off`), else of its `name` (a stem with nothing ASCII in it), else `ref`; unique within the entity, the
  later of two equal ids taking `-2`, `-3`… (`silo_proto::studio::ref_ids`). So a rename leaves the id, and the
  tokens already written with it, alone; name the file for what it is (`refs/crown-off.png`), as the id reads.
  A plain path reads as a reference named by its stem with no `defines`. Its kind is derived, not stored: video
  and audio from the file, else an image (a look, an angle, a variant: the name and `defines` tell them apart). A
  `@characters/mara#hood-down` mention wires it into a card (§3). Entries whose file leaves the root are dropped.
- **Links.** `links` lists entity ids (`props/bhima-mace`; `@` and `world/` prefixes and a trailing `/` are
  read too). A link reads from both ends: an entity's links are those it names, then those whose `links` name it
  (by id), existing entities only, each once, never itself. Nothing is wired from a link: it says what belongs
  together, for the user and the agent.
- Everything is readable and writable by any tool: the agent uses `read`/`write`/`edit` for world text and
  `generate_image {path: "world/characters/mara/sheet.png"}` for sheets. The server watches the folder and the
  block follows.

### 1.1 `studio.toml`

```toml
name = "Harbor Lights"
format = "landscape"                         # the project default: a preset below

[formats.landscape]
aspect = "16:9"
resolution = "1080p"
fps = 24

[formats.vertical]                           # reels, shorts, stories
aspect = "9:16"
resolution = "1080p"
fps = 30

[formats.square]
aspect = "1:1"
resolution = "1080p"

[formats.portrait]                           # feed posts
aspect = "4:5"
resolution = "1080p"

[canvases]
order = ["ep01-pilot", "ep02-undertow", "moodboard"]   # tab order; missing ones go last by name
```

`studio.init` writes these four presets; users and the agent add their own (`[formats.cinema] aspect = "2.39:1"`).
A preset is `aspect` (`W:H`), `resolution` (`480p`/`720p`/`1080p`/`4k`, the short side) and optional `fps`.

### 1.2 `canvas.json`

```jsonc
{
  "version": 1,
  "title": "Pilot",
  "episode": 1,                // optional: marks an episode; the series order is by this number
  "format": "vertical",        // optional override of the project default
  "nodes": [ … ],
  "edges": [ … ]
}
```

Written by the server only (atomically: a temp file renamed over it), with a `rev` bumped on every write. An agent
that edits it with file tools still works (the server reloads on change), but the tools in §5 are the way in.
The camera (pan, zoom) and selection are per client and are not saved in the file.

## 2. Formats

The effective format of a generation is the first that is set, from the most specific to the least:
the shot's frame, the canvas, the project's `format`.

- **Switching:** the header's format chip (§4.1) sets the canvas override (or clears it: "Project default") and,
  from its menu, the project default. `studio.set_format` writes either one.
- **Generation:** a `generate_*` / `generate_media` call aimed at a canvas (§5.2) fills `aspect_ratio` and
  `resolution` (and `fps` and the model's equivalent fields in `generate_media` when its schema has them) from the
  effective format **when the call doesn't set them**. When the model can't produce that aspect, it gets the
  nearest one it can and the result says so, as the media tools already do for options they can't do
  (`Kling 3.0 Pro has no 4:5; made 9:16.`). An explicit value in the call wins.
- **Placeholders** on the canvas use the effective aspect ratio while they wait.

## 3. Model (silo-proto, additive)

```
BlockKind::Studio                         // an older app shows it as Unknown

StudioState {                             // BlockInfo.studio: Option<Box<StudioState>>, in session views
  root: String,                           // absolute; the block never reads or writes outside it
  view: StudioView,                       // what the body shows
  open: Vec<String>,                      // canvas tabs open, in order (canvas names = folder names)
  active: Option<String>,                 // the canvas shown when view is Canvas
  world_section: Option<String>,          // "characters", "places", "style", …
  world_entity: Option<String>,           // an entity shown in detail ("characters/mara")
  agent: Option<AgentLink>,               // the agent block it talks to (as Changes)
}
enum StudioView { Canvas, World, Setup }  // Setup: the root has no studio.toml yet

Project   { name, format: String, formats: BTreeMap<String, Format>, canvases: [CanvasInfo], sections: [Section] }
Format    { aspect: String, resolution: Option<String>, fps: Option<u32> }
CanvasInfo{ name, title, episode: Option<u32>, format: Option<String>, nodes: u32, updated: u64, rev: u64 }
Section   { id, title, kind: overview|style|entities, entities: [EntityInfo] }
EntityInfo{ id, name, summary: Option<String>, sheet: Option<String>, refs: [String], tags: [String],
            looks: [RefInfo],                    // every reference, named, in file order (refs: their paths)
            links: [String] }                    // entity ids: those it names, then those naming it (§1)
RefInfo   { id, path, name, defines: Option<String>, media: StudioMedia }  // a named reference (§1)

Canvas    { name, title, episode, format, rev, nodes: [Node], edges: [Edge] }
Node      { id, x, y, w, h, z?, parent?: String /* a frame */, body: NodeBody }
NodeBody (tag = "kind") {
  Asset  { path, media?: image|video|audio|text,     // media omitted: inferred from the path by the server
           width?, height?, seconds?, prompt?: String },
  Note   { text },                                   // markdown: a prompt draft, a beat, a direction
  Entity { id },                                     // a world entity placed on the canvas (shows its sheet)
  Gen    { makes: image|video|speech|sound|music,    // the card: what it generates
           title?, prompt: String, model?: String,   // model None: automatic (the project's pick, else the default)
           mode?, options: Value,                    // the card's own settings (aspect, length, voice, …); unset: inherited
           status: draft|queued|running|done|failed|stopped, progress?: String, job?: String, error?: String,
           takes: [Take], pick?: u32,                // its results; the picked take is what it shows and outputs
           inputs_used: [String],                    // input keys the picked take was made from (out of date)
           tool?, input?: Value },                   // the last request sent, as a record
  Frame  { title, role: group|scene|shot, order?: u32, format?: String },
}
Take { path, media, width?, height?, seconds?, model?, prompt?, at: u64 }
Edge { id, from, to, role: first_frame|middle_frame|last_frame|reference|video|audio|text|next, label?,
       mention?: bool }                      // made by a mention in the card's prompt (below); omitted when false
```

- Ids are short random strings (`n_3f2a9c`), stable across writes.
- **Wires are inputs.** An edge into a card with a port role is a wire; the role is the port it lands on. `next`
  is not a wire: it orders shots when their frames don't.

  | Card | Ports (top to bottom) | What a wire becomes |
  |---|---|---|
  | Image | `reference`, `text` | reference images (characters, places, style); text is context |
  | Video | `first_frame`, `middle_frame`, `last_frame`, `reference`, `video`, `audio`, `text` | the frames the clip opens on, passes through and ends on; references to keep; a clip to build on; voice lines and sounds; context |
  | Speech / Sound / Music | `text` | the line to read (when the card's prompt is empty) or context |

  Ports take: frames and `reference` an image, `video` a video, `audio` audio, `text` text. Frame ports hold one
  wire (a new one replaces it); the others any number. Wires never loop.
- **Sources:** any node with an output can be wired from: an `Asset` (its file), an `Entity` (its sheet), a card
  (its picked take), a `Note` (its text). A source's **key** names what it outputs now (`path@mtime`, the take's
  path, a hash of the text); a card whose keys differ from its `inputs_used` is **out of date** ("Mara's sheet
  changed"). Nothing reruns on its own.
- **Takes:** every run adds a take and picks it; older takes stay, and picking one is an ordinary edit. `pick` is
  an index into `takes` from 0 (none: the last). A run asked for several (`options.n`) adds them all and picks the
  first of them. A card with no take is a `draft`.
- **Order:** wires reach the model by port (frames first, then references, video, audio, text) and, within a
  port, in the order they were made (their order in `edges`): the first `reference` wire is "image 1". The
  settings panel and the preview show them numbered.
- **Mentions** (`silo_proto::studio::mention`): a card's `prompt` may name its sources as tokens, stored as plain
  text. `@<section>/<id>` names a world entity (`@characters/mara`: its sheet), `@<section>/<id>#<ref id>` one of
  its named references (`@characters/dhritarashtra#crown-off`, §1), `@#<node id>` a node of the same canvas
  (`@#n_3f2a9c`: a card, an asset, a document, a note, an Entity node). A token starts at the start of the text or
  after whitespace or an opening bracket or quote, and runs over the characters an id has, so it ends before
  whitespace and trailing punctuation (`@characters/mara.`, `(@#n_1),`, `@characters/mara's`). A reference id
  (letters, digits, `_`, `-`, at most 64) follows the entity part at once; `@characters/mara#` with nothing an id
  has after the `#` is the entity's token and the `#` text. A token that doesn't resolve (an e-mail, a handle, an
  unknown entity or node, a frame, the card itself) is ordinary text, and so is a whole reference token whose
  reference the entity doesn't have: `@characters/mara#nope` wires nothing, not her sheet, so a misspelt
  reference never quietly becomes the sheet. **A mention is a wire:** the server wires its source into the card
  when the token is written (§5.2), and a run expands it to text the model can use, `Mara Venn (image 1)`,
  `Dhritarashtra, Crown off (image 2: take only his bare head and hair, without the crown)` (§6.2). Tokens
  anywhere but a card's own prompt (notes, titles) do nothing. The parser, `mention::tokens(text) -> Vec<Token {
  range: Range<usize> /* bytes */, target: Target::Entity(id) | Target::Ref { entity, id } | Target::Node(id) }>`
  (`Target::token()` writes it back, `Target::entity()` the entity either names, `Target::look(sections)` the
  entity and `RefInfo`), and the resolution helpers (`Canvas::mention_label`, `mention_sources(target, card,
  sections)`, `mentions_of`, `mention_names(target, from, sections)`; `mention::port_for(media)`,
  `mention::look_label`) are shared by the server, otto and the app. A reference's sources are the Asset nodes
  showing its file; its unwired label is `<entity name>, <reference name>`. `mention_names` with no `sections`
  (the app's optimistic edit) counts any Asset node of a file in the entity's folder as a reference's, so a
  mention wire is kept until the server's answer rather than guessed away.
- **Options** are named as the quick tools name them (`aspect_ratio`, `resolution`, `duration`, `sound`, `quality`,
  `transparent`, `n`, `voice`), plus `style` (false: neither the project's style line nor its motion line is
  added) and, with a model's mode, any field of that model's schema. Unset size fields come from the effective format (§2).
- An `Asset` node may show any file under the root (a plate in `world/` too). A frame is named by its node id.
- A **document** is an `Asset` of media `text` (`.md`, `.markdown`, `.txt`; `StudioMedia::of_path`): a document
  card. Its output is media Text with its **path** (whoever compiles reads the file; the proto crate reads no
  files) and key `path@mtime`, so a card wired from the script goes out of date when the script changes. A
  canvas's **documents** are the text files directly in `canvases/<name>/` (`studio::documents_of`: `script.md`
  first, then by name).
- The port rules, a source's output and key, and out-of-date are pure functions in `silo_proto::studio`, shared by
  the server (validation), the app (ports, badges) and otto (compiling a run).
- `serde(default)` and skip-when-empty everywhere; enums have an `Unknown` (`serde(other)`) for newer peers.

## 4. App

### 4.1 Layout

```
┌ ◈ Studio · Harbor Lights   [Ep 1 Pilot] [Ep 2 Undertow] [Moodboard] [+]      [9:16 ▾] [⌕] [🌐] ┐
│                                                                                               │
│   ┌ Scene 1 · Harbor at dawn ───────────────────────────────────────────┐   ┌───────────┐       │
│   │ ┌ Shot 1 ──────┐   ┌ Shot 2 ──────┐   ┌ Shot 3 ──────┐               │   │ ✎ Note    │       │
│   │ │ [keyframe]   │──▶│ [video ▶ 5s] │──▶│ [░░ 40% ░░]  │               │   │ Mara hears│       │
│   │ │ [video ▶ 6s] │   │              │   │ Kling 3.0    │               │   │ the bell… │       │
│   │ └──────────────┘   └──────────────┘   └──────────────┘               │   └───────────┘       │
│   └──────────────────────────────────────────────────────────────────────┘                       │
│        ▲ reference                                                                            │
│   [Mara · sheet]   [Harbor · sheet]                                    ┌ minimap ┐             │
└──────────────────────────────────────────────────────────────── 3 selected · Ask agent ─────────┘
```

- **Header:** the studio's name, the canvas tabs (episode canvases show `Ep N` before the title, in episode order,
  then the others in `studio.toml` order), `+` (New canvas: title, episode number or none, format), then on the
  right: the **format chip** (the effective format of the active canvas; its menu: Project default, each preset,
  Edit formats… (opens `studio.toml` in a Files block); a second group sets the project default), search (⌕:
  nodes, prompts, entities) and the **World** icon.
- Tabs can be closed (the canvas stays on disk) and reopened from `+` → Open canvas. Renaming a tab renames the
  title; the folder keeps its name.

### 4.2 Canvas

- **Navigate:** two-finger scroll pans, pinch or ⌘-scroll zooms (5 %–400 %, around the pointer), space-drag pans,
  `⌘0` fits everything, `f` fits the selection, a minimap in the corner when the content is larger than the view.
- **Select and edit:** click, shift-click, marquee; drag moves (snaps to other nodes' edges and to frames, which
  adopt what is dropped in them); resize from corners; Delete removes the node (assets: the file stays in
  `assets/`; Delete File… from its menu moves it to the Trash); `⌘D` duplicates; `⌘Z` undoes the block's own edits.
- **Nodes:**
  - Image: the image, fitted. Video: its poster frame (as the agent block's video tile), the duration, playing
    muted on hover; at most 4 play at once. Audio: the agent block's compact player. Double-click opens the
    shared media viewer (MEDIA.md §5.3, §6.4: zoom and pan, the toolbar, Copy, Save…, the footer with the take's
    model and prompt, the full video player) over the whole block body, opaque once open, above every canvas
    overlay (Documents, toolbar, selection actions, minimap, panels) and taking all input, with `[`/`]` walking
    the canvas's images and videos in reading order (assets, entities' sheets, cards' picked takes; a card's new
    take shows at once); Esc gives the board its focus back, the node shown last selected. Built:
    `studio/viewer.rs` (items, media from the store, `layers`).
  - Card (Gen): a slim header (kind icon, title, the model or **Out of date**; the title truncates before the
    model does), the picked take edge to edge at its own shape (a draft: an empty face at the effective aspect
    ratio), the prompt (editable in place) and a small **Run**. The prompt shows **collapsed to 4 lines** with an
    ellipsis; the card being edited, or the single selected card, shows it in full up to 12 lines (the editor
    scrolls beyond); the board decides which, so the card's height and hit testing follow what is drawn, and text
    never runs under the footer. Its input ports are round modules floating left of the card, one icon each
    (hover names them, wired ones are filled); the output floats on the right. While it runs: a shimmer over the
    face, the elapsed time and the provider's progress, Run becomes Stop. Failed: the error wrapped on the face
    (4 lines, the full text in a tooltip) and Retry; a run refused for want of a provider (`error_code`
    `no_provider`) says "No image provider connected" (video, audio) and the footer's **Connect provider** opens
    the media setup card for its kind, then runs the card (as Run does on that refusal). `1/3` on the header
    steps through takes.
  - Note: rendered markdown (bold, italics, lists, inline code, links as text, modest headings), scaled with the
    zoom and clipped to the note; double-click edits the raw text inline.
  - Document (a text asset, §3): a title bar with the file name over the file rendered as markdown, clipped with
    a fade at the bottom, re-read when the file changes. Double-click, Enter or Open shows it in the document
    panel; its menu: Open, Open in Files, Make from this (cards through `text`), Delete (the card; the file
    stays). Dragging from its output offers any card, wired into `text`.
  - Entity: the entity's sheet with its name; clicking it opens the World view on it.
  - Frame: a titled rounded region; scene and shot frames show their order (`Scene 1`, `Shot 3`). Shots inside a
    scene read left to right.
- **Wiring:** drag from any node's output dot onto a port, or onto a card's body to use its first free port that
  fits (an image onto a video card: first frame, then reference). The server refuses what can't be (a port the
  card lacks, the wrong media, a loop). Click a wire to select it, Delete unwires. Wires are curves.
- **Drag to create:** drop a wire on empty canvas and a menu offers the cards that fit (from an image: Image or
  Video; from a video: Video; from a note: any). The new card is made already wired, its prompt focused.
- **Canvas prompt:** double-click empty canvas, or `/` or `G`: pick a kind (Image, Video, Voice, Sound, Music),
  type, Enter makes the card there and runs it (⌥Enter: the card only, to wire inputs first).
- **Mentions in prompts** (§3): typing `@` in a card's inline prompt or the canvas prompt opens the composer's
  `@` popup (its rows and drawing, `agent::mention::list`), with data from the app alone: the world's entities by
  name with their kind (`Character`), then under "On the canvas" the nodes `Canvas::mention_label` resolves for
  this card (other cards by title, notes by first line, documents and other files by name, with their frame;
  not frames, the card itself, empty notes or Entity nodes). A bare `@` lists them in that order, a typed query
  filters each group best match first (name or id), and lists an entity's named references whose name or id
  holds the query right under it (`crown` → `Dhritarashtra · Crown off   Look`); `@#…` lists nodes only. A picked
  entity gets no trailing space: `#` typed right after its chip reopens it as `@characters/mara#…`, listing its
  references (`Crown off   Look · Dhritarashtra`, from `EntityInfo.looks`, filtered by name or id), and a pick
  writes `@characters/mara#crown-off`. ↑/↓, Tab and ⏎ pick, a click picks,
  esc closes it until that `@` goes. A pick writes the token (`@characters/mara`, `@#n_3f2a9c`). A token that
  resolves is drawn as a chip (the composer's) reading the entity's name, a reference's entity and name
  (`Dhritarashtra · Crown off`, the icon of its media: image, video, audio) or the node's label with its kind's icon,
  one unit for the caret and backspace, in the editor and on the card's resting face; the stored prompt is the
  text with its tokens, and what doesn't resolve stays text. A saved mention whose source has no wire into the
  card (unwired by hand, or a video or audio source waiting for a hand-drawn wire) is a quiet chip, dashed, with
  the tooltip "… is mentioned but not wired into this card". The server's `CanvasEdited.mentions` are read on
  the ack: a wired one shows as its wire; a `not_wired` one's `why` is the canvas's transient notice (with any
  failed op's message). Undo of a prompt edit puts the prompt back (the server then removes the wire its mention
  made; applied optimistically, the wires `Edge.mention` of tokens gone are dropped at once) and removes the
  Entity nodes the sync added (`mentions[].node`, added to the batch's undo step on the ack); redo adds them back
  and sets the prompt, which wires again. A selected or hovered wire with `Edge.mention` is labelled "From a
  mention in the prompt". The settings panel doesn't edit the prompt (its preview is otto's). Built:
  `studio/mentions.rs` (rows, chips, wired), `studio/canvas/mentions.rs` (popup state, keys, placement).
- **Toolbar** (bottom centre): Select `V`, Note `N`, Frame `F`, Prompt `G`, Import.
- **Documents** (a button at the board's top-left, with a count): the active canvas's documents (§3), listed
  with `files.list` on the Studio block, a mark on those pinned. Clicking one shows it in the **document panel**
  (and reveals its card when pinned): a 420 pt panel on the right, full height, closed with Esc or ×, showing the
  file rendered (tables too), its name, **Edit** (the World view's inline editor and `files.write` save, with its
  conflict rules), Open in Files, and **Add to canvas** (a document card at the view's centre; **Show on
  canvas** when it is pinned). It follows the file (`studio.changed` for the canvas, rev-less, when a document in
  its folder changes). The panel also opens from a document card and from outside (`StudioPane::open_document`).
  Built: `studio/documents.rs` (button, list, panel; the store's `documents(canvas)`), `canvas/markdown.rs` (notes
  and document previews at the zoom), the board's `Shape::Asset(Text)` (`layout::DOC_HEADER` title bar, `face` the
  preview), `Effect::OpenDocument`, `Action::OpenDocument` and `Board::add_document`.
- **Selection actions** float over a single selected node: a note gets Visualize (an image card wired from it);
  an image gets Animate (a video card with it as first frame), Edit (an image card with it as reference); a video
  gets Extend (a video card wired into `video`); a card gets Run again (a new take), Takes, and Settings.
- **Card settings** (a slim panel beside the selected card): title, model (Automatic, or any model of its kind,
  with those that can't take what is wired dimmed and the reason), format (Inherited: 9:16, or its own), length,
  resolution, voice, "Use the project's style" (on: the style line of `world/style.md` is added to the prompt,
  and on a video card its motion line after it),
  its inputs (click to jump, × to unwire), and the request it would send (§6.2's preview).
- **Drop:** files from Finder or a Files block are copied into `assets/` and placed at the drop point; an entity
  dragged from the World panel places an Entity node.
- **Context menu:** on empty canvas: Generate here (Image, Video, Voice, Sound, Music), Note, Frame, Add from
  World…; on a node: Open and Open in Files for documents, Run / Run again and Settings for cards, Make from this (a new card to its right, already
  wired), Ask agent about this, Set as entity sheet…, Reveal in Files, Copy, Save…, Delete.
- **Ask agent** (selection bar, or ⌥⌘↩): focuses the linked agent block (else splits a `studio` agent block off
  this one and links it) and puts the selection in its composer as mentions (`@canvas:ep01-pilot#n_3f2a9c`), with
  the cursor after them, so the user writes what they want ("redo these with a warmer grade").
- **Live:** `studio.changed` for the active canvas re-reads it; a gen's progress updates only its node.

### 4.3 World view

The World icon toggles the body between the active canvas and the World view (the tabs stay; clicking one goes
back). The command palette's "Toggle World" toggles too (no shortcut: `⇧⌘W` is Close Window everywhere).

```
┌ ◈ Studio · Harbor Lights   [Ep 1 Pilot] [Ep 2 Undertow] [+]                 [9:16 ▾] [⌕] [🌐●] ┐
│  Overview   Style   Characters·4   Places·3   Props·2   Factions·1   [+ Section]               │
│  ───────────────────────────────────────────────────────────────────────────────────────────  │
│   ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌ + ─────┐                                     │
│   │ sheet  │ │ sheet  │ │ sheet  │ │ (none) │ │  New   │                                     │
│   │        │ │        │ │        │ │        │ │character│                                    │
│   ├────────┤ ├────────┤ ├────────┤ ├────────┤ └────────┘                                     │
│   │Mara    │ │Teo     │ │Old Bell│ │Inspector│                                              │
│   │Salvage…│ │Her bro…│ │Harbor… │ │         │                                              │
│   └────────┘ └────────┘ └────────┘ └────────┘                                                 │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Switcher:** Overview, Style, then each section with its count, then `+ Section` (makes a folder).
- **Overview / Style:** the markdown rendered, an Edit button (inline editing, saved with `files.write`'s rules),
  and on Style a strip of its reference images (`world/style/` if present).
- **Entity grid:** cards with the sheet (a placeholder when there is none), the name and the summary. `New …` asks
  for a name and makes the folder and file; from its menu, "Create with agent" sends the linked agent
  `Create a new character for this world: <what the user typed>`.
- **Entity detail** (click a card): the sheet large and whole (contained at its own aspect, never cropped; a 4:3
  placeholder until it loads), the refs strip, the frontmatter as fields, the description rendered and editable,
  "Used in" (canvases and nodes referencing it), and actions. The refs strip draws each ref by its file: an image
  as a thumbnail; a video (`mp4`, `mov`, `m4v`, `webm`: a video sheet) as the canvas draws one, its poster frame
  with its length and a play mark; audio (`mp3`, `wav`, `m4a`: a voice sample) as the canvas's compact player,
  played in place. The actions follow the asset workflow (a sheet, often fixed by hand, then a video sheet, then
  a character's voice); the agent does the generating, each button only sends the linked agent a prepared
  request naming the entity and its id (`Mara Venn (world entity characters/mara)`), and depends on the
  entity's section:
  - characters: Generate sheet (`Generate a character sheet for …: three views in one image, and set it as its
    sheet.`), Generate video sheet (`Generate a 360° video sheet for … from its sheet, in which it says one line
    (its voice reference), and add it to its refs.`), Cast voice (`Cast a voice for …: offer two or three with a
    sample line, then set the one I pick.`);
  - props: Generate sheet (`Generate a prop sheet for …: two views in one image, and set it as its sheet.`),
    Generate video sheet (`… from its sheet and add it to its refs.`);
  - places: Generate sheet (`Generate the establishing image for … and set it as its sheet.`), Generate angles
    (the reverse angle and the variants its scenes need, added to its refs);
  - any other section: Generate sheet (`Generate a <one> sheet for … and set it as its sheet.`, `<one>` the
    section's title made singular).

  With a sheet already, the first reads Regenerate sheet. Beside them, one model chip per kind of job they use
  (§4.6: Image for a sheet or angles, Video for a video sheet). Under them, Approve (§4.6, with a sheet) and Add to
  canvas. The name has the entity's state marks under it (§4.6). Wide (an inner width of
  560 pt or more), the sheet and refs are a column on the left (half the width, 240–640 pt) and the name,
  summary, fields, actions, description and "Used in" are on
  the right, each column scrolling on its own; narrower, they stack in one scrolling page. The description is
  drawn at its full height, so its column (or the page) is the only thing that scrolls; a scroller of its own
  inside the page would scroll along with it. Buttons hug their labels.
- **Named references and links.** The sheet carries a "Sheet · default look" badge (what `@<entity>` wires).
  Under it, "References" (with their count) lists `EntityInfo.looks` in file order as tiles (an image thumbnail,
  a video's poster tile, audio's player), each with its name under it and, when set, what it `defines` as a
  second muted line (two lines at most; the tooltip has it whole, or says it isn't said yet). The tiles wrap,
  so they read the same in the side-by-side page and the stacked one. Right-click: Rename…, What it defines…
  (each the pane's one-line dialog, filled with the current text; empty resets the name to the file's stem or
  clears what it defines; a rename keeps the id, so the mentions written with it still wire), Set as sheet (images), Move earlier / Move later (where it can go), Reveal in Finder
  and Open in default app (Open in Files on a remote host), Copy mention (`@characters/dhritarashtra#crown-off`),
  Remove (the file stays); each a `studio.entity_ref` change (§5), shown when the server answers. An "Add
  reference" tile ends the list: "Ask the agent…" sends "Add a look for <name> (world entity <id>): ask me which
  look I want …" (with the project's Image pick, like the page's other asks; the app has no way to put a
  prepared sentence in the composer without sending it), and on this Mac "File…" picks files, each added with
  `RefChange::Add` (copied into `refs/`). "Goes with" (between the fields and the actions) lists
  `EntityInfo.links` as chips with the linked entity's sheet thumbnail and name: a click opens it, right-click
  offers Remove link; `+` opens the world's other entities by section (props first on a character's page,
  characters first on a prop's, the world's order otherwise; those already linked left out), a pick links it
  (`studio.entity_link`). The `links` field isn't repeated among the fields. On the World grid a prop that
  links to exactly one character says whose it is beside its name ("Bhima's"); otherwise nothing.
- **Viewing:** clicking the sheet or an image or video ref opens the media viewer (§4.2's, hosted by the pane
  over the whole body; a video plays) on the entity's sheet then those refs, so `[`/`]` step through them, with
  zoom, Copy and Save; Esc closes it and the pane has the focus again. Leaving the entity (another one, its
  section, a canvas) closes it. Audio refs are not in it.
- **Fixing by hand:** the sheet's right-click menu offers Reveal in Finder and Open in default app (on a remote
  host, where neither can, Open in Files instead); a reference's has them after its naming, sheet and order rows
  (Set as sheet: images only, a video or audio ref can't be the sheet) and before Copy mention and Remove. A file changed on disk by another program comes back as
  `studio.changed` `world` (the server's watcher): the page reads the entity again and every asset under
  `world/` is read again when next drawn, so the new image shows without reopening; canvases already read that
  show entities are read again too, so the cards wired from it show Out of date (the server compares the sheet's
  mtime with the take's `inputs_used`).

### 4.4 Setup

A Studio opened on a folder without `studio.toml` shows Setup: name, default format (the presets as visual
aspect tiles), and two buttons: **Start empty** (`studio.init`) and **Start with the agent** (`studio.init`, then
splits a `studio` agent block, linked, that begins the onboarding interview, §6.4).

### 4.5 Block core

A `BlockPane::Studio` arm in every shared match (docs/BLOCKS.md checklist), `Kind::Studio` (glyph `clapperboard`,
`has_folder: true`), the launcher's Studio row, New Studio Tab/Pane, ⌘P "Open Studio". `unsaved`: an inline edit in
progress. Header can hide like Files'.

### 4.6 Production view (where the project stands)

A studio is a production with an order: assets, script, shots, sound, cut. The canvas shows one episode's graph and
the World view one entity at a time; neither says how far the whole thing is or what to do next. The Production
view does. Everything on it is **derived** from the files each time (`studio.production`, §5): there is no checklist
to keep in step, so it can't go stale and the agent has nothing to maintain.

- **Opening it:** the studio's name at the header's left is a button (`StudioView::Production`). It is also what
  the body shows when no canvas is open, and where Setup lands.
- **Next up** (top): one to three steps, the first things not done, each a button. A step either sends a prepared
  message to the linked agent ("Make Mara's sheet", "Write episode 1's script", "Make the 5 keyframes scene 2
  still needs") or opens what the user must decide themselves ("Review and approve Mara's sheet" → her page).
- **Assets:** the world's entities by section, a row each: sheet thumbnail, name, and its states as small marks:
  `Sheet`, `Approved`, and by kind `Voice` (characters), `Video sheet` (characters, props). A missing one is
  hollow; `Changed` replaces `Approved` when the sheet file is newer than the approval. Beside its canvases
  count, how many references it has beyond the sheet when not zero (`3 looks`, from the world listing). A prop
  that goes with exactly one character also sits under that character's row, indented (it stays in its own
  section too). A click opens its page.
- **Episodes:** each canvas with an episode number (then the others), a row each with its stages:
  `Script` · `Shots n` · `Keyframes a/n` · `Video b/n` · `Sound` · `Cut vN`; plus `k out of date`, `k failed`,
  `k running` when not zero. A click opens the canvas. With a cut, **▶ Play** opens the newest
  `canvases/<name>/out/<name>-v<N>.mp4` in the media viewer (§4.2's; `[`/`]` step through older versions).
- **Models:** the project's picks for Image, Video and Speech (`studio.toml` `[models]`): a chip each reading the
  model's name or `Automatic`, its menu the connected models of that kind (the card settings' list) and
  Automatic; a pick is written with `studio.set_models`.

**Approval.** An entity's sheet is approved by the user: **Approve** on its page (and `Approved ✓`, a click
withdraws it). It is the frontmatter's `approved: <RFC 3339 UTC time>`, stamped by `studio.entity_approve`. A sheet
file changed after that time reads **Changed since approved** with **Approve again**. Approval is advice, never a
lock: cards wired from an unapproved sheet still run. It tells the agent what is final (§6.4) and the user what is
left to decide.

**Readiness on the World grid:** an entity card shows the same marks as its Production row, under the summary.

**Model on the entity page:** beside the Generate buttons a chip reads the model that job will use (a sheet or
angles: the Image pick; a video sheet: the Video pick; a voice: none), with the Models menu above. The prepared
message names it (`… Use soul-2.`) when there is a pick; with Automatic the agent chooses and says why.

**Built** (`studio/production.rs`, the store's `production`, `approve`, `set_model` and `models`):
- The view shows when `view` is `Production`, and when it is `Canvas` with no canvas active (`shows_production`);
  Setup's Start lands on it. The header's name is highlighted while it shows. The page is one scrolling column at
  most 1040 pt wide, centred, with a title line (`5 assets · 2 episodes`) above the sections.
- **Next up:** the steps as buttons in a wrapping row, the first in the accent; each has a tooltip with the message
  it sends or what it opens. With none: a line saying nothing is waiting. A step without `ask` opens `entity`,
  else `canvas`.
- **Assets:** a compact row per entity (a 44 pt sheet thumbnail, the name, `Not used yet` / `n canvases`, the marks
  under them), in a grid of rows at least 300 pt wide (one column beside an agent block, two or three alone), under
  a heading per section (`Characters · 4`) in the world's section order. Empty: a dashed box saying what assets are,
  with Create a character with the agent…, New character… and Open the World.
- **Episodes:** a row per canvas (the server's order): `Ep 1 · Pilot`, the stages as marks (`Keyframes 3/5` stays
  hollow until every shot has one; a stale cut is `Cut v3 · stale` in the warning colour), the counts in the warning
  colour under them, and **Play** at the right. From an inner width of 640 pt the title is a 200 pt column beside
  the marks; narrower, the marks go under it. A canvas that isn't an episode and has no script, shots or cut reads
  `Not an episode`. With no episode: a dashed box saying what an episode is, with the `episode` step's button when
  there is one and New episode N… (New canvas with the next number).
- **Play** opens the pane's media viewer (`viewer.rs` `CutHost`, as the World view's `EntityHost`) on the newest
  cut, titled `Ep 1 · Pilot · v3` (`(stale: takes changed since)` added when stale), then the older versions newest
  first, so `]` steps back through them. It closes when the Production view goes or the cut does; closing gives
  the pane the focus.
- **Marks** (`marks(Readiness)`), the same on the Production row, the World card (under the summary) and the
  entity page (under the name): `Sheet`, `Approved` (or `Changed`, warning colour), `Voice` where `voice` is
  present, `Video sheet` where `video_sheet` is; done ones filled with a check, missing ones hollow. The production's
  row is used when read (it alone knows a character doesn't speak), with the approval of the entity as last read;
  before it, the world's listing (a character always has a Voice mark; a video sheet is a video among its refs).
- **Approval:** `Approve`, `Approved ✓` (a click withdraws it) or `Changed since approved` + `Approve again`, under
  the Generate buttons, only with a sheet. Not optimistic: the answer's `EntityInfo` is shown at once in the
  section, the detail and the production row; dimmed while the call is out.
- **Models:** the store lists otto's models per kind (`media.models`, once; again after a provider is connected
  from a card), shared with the card settings panel. The menu: Automatic, the current pick when otto doesn't list
  it as connected (`… (not connected)`, checked), then the connected models (`Name · Provider`); a note row while
  they load, when otto can't be reached, or when none is connected. Speech lists otto's audio models (as a speech
  card's settings do). The chip reads the model's name, its handle without the provider before the list comes, or
  Automatic. The message names the pick by its handle.

## 5. Server API (feature `studio`)

Every call takes `block_id`. Paths are root-relative and stay under the root (Files' rules: canonicalised, no `..`,
no escaping symlinks). View rights to read, interact rights to change anything.

| Method | Params | Result |
|---|---|---|
| `studio.project` | — | `Project` (`not_found` when there is no `studio.toml`: the app shows Setup) |
| `studio.init` | `name`, `format` | `Project`; writes `studio.toml`, `world/` with `overview.md`, `style.md`, `characters/`, `places/`, `props/`; never overwrites |
| `studio.set_format` | `canvas?`, `format: Option<String>` | `Project`; with `canvas`: its override (`None` clears), else the project default |
| `studio.set_formats` | `formats: {name: Format}` | `Project`; adds or replaces presets |
| `studio.canvas` | `name` | `Canvas` |
| `studio.canvas_create` | `title`, `episode?`, `format?`, `name?` | `CanvasInfo`; the folder name is the slug (`ep01-pilot` for an episode) |
| `studio.canvas_update` | `name`, `title?`, `episode?`, `format?` | `CanvasInfo` |
| `studio.canvas_delete` | `name` | moves the folder to the Trash (macOS), else deletes; asks in the app first |
| `studio.canvas_edit` | `name`, `ops: [Op]`, `base_rev?` | `{rev, ids: [String], mentions?}`: ids of added nodes and edges, in op order; what the prompts' mentions wired |
| `studio.world` | — | `[Section]` |
| `studio.entity` | `id` | `Entity { info: EntityInfo, fields: Map, body: String, used_in: [{canvas, node}] }` |
| `studio.entity_create` | `section`, `name`, `fields?`, `body?` | `EntityInfo` |
| `studio.entity_update` | `id`, `fields?` (merged), `body?` | `EntityInfo` |
| `studio.entity_approve` | `id`, `approved: bool` | `EntityInfo`; sets the frontmatter's `approved` to now (RFC 3339 UTC to the second, `2026-11-25T09:30:00Z`), or removes it, every other line as written (withdrawing an absent one writes nothing); approving an entity with no sheet is `bad_request` |
| `studio.entity_ref` | `id`, `change: RefChange` (below) | `EntityRefEdited { info: EntityInfo, ref?: String }`: one change to its named references, written in `refs` with every other line as written (`ref`: the changed reference's id as it is now; none after `remove`). A list not written yet (the `refs/` folder's images) is written out first. `not_found` for an unknown entity or reference id |
| `studio.entity_link` | `id`, `other`, `linked: bool` | `EntityInfo` of `id`; linking writes `other` into `id`'s `links` unless either already names the other (nothing written); unlinking takes it out of both files' `links`, wherever it is declared (a list left empty goes); linking an entity to itself is `bad_request` |
| `studio.production` | — | `Production` (below): derived from the world's files and every canvas, nothing stored |
| `studio.set_models` | `models: {kind: handle or null}` | `Project`; merges into `studio.toml`'s `[models]` (`null` removes a key: Automatic), keeping the file's other text and comments (a replaced value keeps its trailing comment); makes the table only to add a pick; kinds are letters, digits, `-`, `_` |
| `studio.asset` | `path`, `offset?`, `length?` (≤ 4 MiB) | `{mime, size, offset, data_base64}`: chunked reads for any media under the root |
| `studio.import` | `canvas`, `from` (a root path or an absolute path the caller may read) | `{path}` copied into the canvas's `assets/` |
| `studio.set` | partial `StudioState` | the state, broadcast as `block.changed` |

```
Op (tag = "op")
  add_node    { node: Node (id optional) }
  update_node { id, x?, y?, w?, h?, parent?, body?: partial }      // partial bodies merge: a card's prompt, pick, status, takes
  remove_node { id }                                               // also its edges; children of a frame stay
  add_edge    { edge: Edge (id optional) }
  remove_edge { id }
  layout      { ids, as: row|column|grid, in?: frame, gap? }       // so the agent can place many nodes without coordinates
  tidy        { frame? }                                           // re-flow so nothing overlaps (none: the whole canvas)

RefChange (tag = "op"; `ref` is a reference's id)
  add      { path, name?, defines?, at? }   // path: root-relative; under the entity's folder it is listed where it is,
                                            // else copied into its refs/ (a taken name gets -2, …; absolute and ~/
                                            // paths for the owner only, as studio.import); with a name, the copy is
                                            // named after its slug when no file in refs/ has that stem ("Crown off":
                                            // refs/crown-off.png), as the stem makes the id. A file listed already
                                            // is named and described in place. at: its index (default: the end)
  rename   { ref, name? }                   // the id stays (the file's); none or empty: back to the file's stem
  describe { ref, defines? }                // none or empty: no job
  move     { ref, to }                      // to: the new index (past the end: last)
  remove   { ref }                          // off the list; the file stays
```

An entry gains the table form when it gets a `name` or `defines` and goes back to a plain path when it has
neither (a table's other keys keep it a table). A list written one item a line stays so, a flow list (`[a, b]`)
stays on one line until it holds a table; a table's keys are written `file`, `name`, `defines`, then the rest.

```
Production
  assets:   [AssetState { id, name, section, sheet?: path, approval: none|approved|changed,
                          voice?: bool        // characters only: `voice:` names a provider/voice; absent when `voice`
                                              // says it doesn't speak (none, false, silent, mute)
                          video_sheet?: bool  // characters and props: a video among `refs`, or in its `refs/` folder
                          used: u32 }]        // canvases using it, by `used_in`'s rules (§5 Server)
  episodes: [EpisodeState { canvas, title, episode?, script: bool,       // canvases/<c>/script.md, not blank
                            shots: u32,                                   // shot frames
                            keyframes: u32, videos: u32,                  // shot frames holding (at any depth) an image /
                                                                          // a video: a card's picked take, or an asset
                            sound: u32,                                   // speech, sound and music cards with a take,
                                                                          // and audio assets, anywhere on the canvas
                            out_of_date: u32, failed: u32,
                            running: u32,                                 // queued or running
                            cut?: Cut }]                                  // every canvas, episodes first (tab order)
  models:   { kind: handle }                                             // `[models]`
  next:     [Step { kind, text, ask?: String, entity?: id, canvas?: name }]  // at most 3, in production order
Cut { path, version: u32,                // the highest N of canvases/<c>/out/<c>-v<N>.mp4 (v10 beats v2)
      older: [path],                     // the other versions, newest first
      at: u64,                           // its mtime, unix ms
      stale: bool }                      // a picked take of a video, speech, sound or music card is newer
StepKind: onboarding|sheet|approve|voice|episode|script|shots|keyframes|video|cut|out_of_date
```

`EntityInfo` gains `approval` (as above; `none` without a sheet) and `voice: bool`; `Project` gains `models`. An
`approved` the server can't read as a time (`true`, a hand-written note) counts as approved with nothing to
compare; `false`, `null` or empty as not. `changed`: the sheet file's mtime is more than 2 s after the approval.

**Next steps** (`production::next_steps`, a pure function of the state above), in order, the first three that apply:
1. `onboarding`: `overview.md` says nothing beyond its headings and the template `studio.init` writes.
2. `sheet`: a character, place or prop used on a canvas (with no canvas yet, any) without a sheet. Custom sections
   need none.
3. `approve`: such an entity's sheet not approved, or changed since (the user's step: `entity`, no `ask`).
4. `voice`: such a character that speaks (`voice` present in `AssetState`) without one.
5. `episode`: no canvas with an episode number (a moodboard doesn't count).
6. Each episode at its stage: `script` (no `script.md` and no shot frames: a canvas already broken into shots goes
   on without one), `shots` (none), `keyframes` (shot frames with neither an image nor a video: a shot made
   straight to video needs no keyframe), `video` (shot frames without a video), `cut` (none, or `stale`). Earlier
   stages of every episode come before later ones, then by episode number, so a missing script of episode 3 is
   named before the video of episode 1.
7. `out_of_date`: cards out of date, on any canvas.

`text` is the button's label ("Make Mara Venn's sheet", "Write episode 1's script", "Make the 5 keyframes episode 1
still needs", "Review and approve Old Quay's sheet"). `ask` is a whole message naming the canvas or entity by id, in
the voice of the entity page's buttons ("Generate a character sheet for Mara Venn (world entity characters/mara):
three views in one image, and set it as its sheet.", "Break the script of episode 1 ("Pilot", canvas ep01-pilot)
into scene and shot frames on its canvas, with a note on each shot.").

**Cost:** `studio.toml`, the world (a folder listing per section and entity, each entity file read, each sheet and
`refs/` folder looked at), then each canvas's `canvas.json` parsed once, a stat per wired file of each card with
takes (as `studio.canvas`'s out of date), and `script.md` and `out/` per canvas. Measured in a test with 16
canvases (12 of 40 cards with takes): about 10 ms per call. Cheap enough to call on every change notice.

**Change notices:** no new one. The app refetches `studio.production` on any `studio.changed`: `world` (an
approval, which the server sends at once; an entity file, a sheet or ref written; `overview.md`), `project`
(`studio.set_models`, `studio.toml`, a canvas added or removed) and `canvas` (`canvas.json`, its `assets/`, its
documents such as `script.md`, and its `out/`: a new cut).

- **Placement without coordinates:** a node added without `x`/`y` goes to the right of the last node in its frame
  (or of the canvas's content), sized for its media at the effective aspect. Agents rarely need coordinates.
- `base_rev`: when the canvas moved on, the ops are still applied (they're by id), and the result says the canvas
  had changed. A removed id in an op is `not_found` for that op only; the others apply.
- **Events:** `studio.changed { block_id, what: project|world|canvas, name?, rev? }`, debounced 150 ms, from the
  Files watcher on the root (so the agent's file edits show) and from the server's own writes.
- **`compat`:** `feature::STUDIO = "studio"`; an older server answers `method_not_found` and the launcher hides
  Studio.

**Built** (types in `silo_proto::studio`, not glob re-exported from `control`; differences from §3 and the above):
- `StudioState` lives in `studio.rs` (`BlockInfo.studio`); `StudioView` defaults to `Canvas` and has `Unknown`.
  `StudioState::new(root)`. Every enum (`StudioView`, `SectionKind`, `StudioMedia`, `GenStatus`, `FrameRole`,
  `EdgeRole`, `Arrange`, `NodeBody`, `Op`, `StudioChange`) has `Unknown` (`serde(other)`).
- Coordinates are `f64` canvas units; `z: Option<i32>`. `add_node` takes a `NewNode` (`id`, `x`, `y`, `w`, `h`
  optional) and `add_edge` a `NewEdge` (`id` optional). The asset node's media enum is `StudioMedia`.
- `EntityInfo` adds `file` (the markdown file); `EntityInfo.sheet`/`refs` and every other path on the wire are
  root-relative (`world/characters/mara/sheet.png`), not entity-relative as in the frontmatter. `Entity.fields`
  is `BTreeMap<String, Value>` (the frontmatter as JSON); `used_in: [EntityUse { canvas, node }]`.
  `CanvasInfo.updated` is unix ms.
- `studio.world` answers `StudioWorld { sections }` (an object, not a bare array). `studio.canvas_delete` answers
  `{}`. `studio.init`'s `format` is optional (default `landscape`). `studio.set_format { format: null }` without
  `canvas` is refused.
- `studio.canvas_update`'s `episode`/`format` and `studio.set`'s `active`, `world_section`, `world_entity`, `agent`:
  absent keeps, `null` clears. `entity_update`'s `fields` merge with `null` removing a key; `body` replaces.
- `update_node`'s `parent: null` moves out of any frame; its `body` is a JSON merge patch (RFC 7396) on the body
  (`kind` can't change). `layout`: `{ids, as: row|column|grid, in?, gap?}`.
- `canvas_edit` answers `CanvasEdited { rev, ids, moved, failed: [OpFailed { op, code, message }], mentions:
  [MentionSynced] }`: `ids` has one entry per `add_node`/`add_edge` op in op order (empty for one that failed);
  `moved` is "`base_rev` was behind"; `mentions` (omitted when empty) is what the mention sync below did.
- **Mentions as wires.** After an edit's ops, every card whose `prompt` they set or changed (`add_node`,
  `update_node`, any path that writes it) has its tokens (§3) compared with its prompt before the edit, once per
  source. A token **written** whose source resolves: an entity's Entity node on the canvas (the first one), else a
  new one added in the card's frame (beside a shot frame instead, in its scene or at the top level, where the
  canvas keeps a scene's Entity nodes): left of the card or shot it feeds, level with its top, in a column going
  down past what is already there, since wires run left to right; without room there, by the usual placement. No
  overlap. A reference (`@characters/mara#hood-down`): the first Asset node showing its file, else a new Asset
  node of it placed as an Entity node is (the plain `@characters/mara` in the same prompt still wires the Entity
  node: both go in). A `#node`: that node. If that
  source isn't wired into the card at any port yet, an edge goes in at the default port for what it outputs
  (`mention::port_for`: an image, entity sheet or image card, to `reference`; a note or document to `text`) with
  `mention: true`. A video or audio source gets no wire (which field it is for is the user's call): it resolves
  through whatever wire the user draws. A token **removed** (no token for that source left) removes the edge from
  that source only when `mention` is set; an Entity or Asset node the sync added stays. `wire_check`'s rules apply (a port
  the card lacks, a loop): the edit still succeeds and the report says why. A wire deleted by hand while its token
  remains stays deleted until the prompt changes that token again; deleting the source leaves the token as text.
  The sync is part of the same write (one rev, one undo step) and is reported as `MentionSynced { card, token,
  did: wired|unwired|not_wired, edge?: Edge, node?: String /* an Entity or Asset node it added */, why? }`: `wired` with the
  edge made, `unwired` with the edge as it was, `not_wired` with the reason. Undoing one is the inverse ops (the
  prompt back, the added Entity node removed), whose own sync takes the wire away.
- `studio.asset` answers `StudioAsset { mime, size, offset, data_base64 }`, `StudioAsset::MAX_CHUNK` 4 MiB;
  `studio.import` answers `StudioImported { path }`.
- `studio.changed` is `Event::StudioChanged { block_id, what: project|world|canvas, name?, rev? }`.
- Helpers: `Project::format_for(&canvas, frame) -> Option<(name, Format)>` (§2: the frame's format walking up its
  parent frames, then the canvas's, then the project's; names without a preset are skipped) and
  `Format::aspect_ratio() -> Option<(f64, f64)>` (`"2.39:1"` → `(2.39, 1.0)`).
- `MentionKind::Entity` (target `characters/mara`) and `MentionKind::Canvas` (`ep01-pilot` or `ep01-pilot#n_3f2a9c`).
- **Cards and wires:** `NodeBody::Gen { makes: GenKind, title?, prompt, model?, mode?, options, status, progress?,
  job?, error?, takes: [Take], pick?, inputs_used, tool?, input? }` (`GenStatus::Draft` is the default; `Gen.outputs`
  and the `output`/`variant` roles are gone; `middle_frame` and `text` added). `Take { path, media, width?, height?,
  seconds?, model?, prompt?, at }` (`studio::Take`; `api.rs` imports `changes::Take` explicitly against the glob
  clash). Pure helpers: `GenKind::{ports, media, label}`, `SourceMedia { image, video, audio, text }`,
  `EdgeRole::{is_port, accepts, single, name}`, `default_port(makes, media, taken)`, `NodeBody::take()` (the picked
  take: `pick`, else the last), and on `Canvas`: `node`, `source_media`, `label_of`, `output_of(id, &[Section]) ->
  SourceOutput { media, path?, text?, label, pending, problem? }`, `inputs_of` (by port, then in `edges` order),
  `source_key(id, sections, mtime)` (`path@mtime`, the picked take's path, `note:<fnv64 hex>`),
  `input_keys(id, sections, mtime)`, `out_of_date(id, keys_now)` (sorted keys differ; drafts and running cards
  never), `out_of_date_why(id, sections, mtime)` and `wire_check(from, to, role) -> Result<replaced wire ids,
  reason>`. **mtimes are unix ms** everywhere (the server computes out-of-date with them; otto's `inputs_used`
  must use the same).
- `studio.canvas` fills `Canvas.out_of_date: [node id]` and `out_of_date_why: {id: [reason]}` at read time (never
  saved). `canvas_edit` checks every edge with `wire_check` (a port the card lacks, the wrong media, a duplicate, a
  loop fail that op with the reason); a frame port's new wire replaces the old one in its place in `edges`.
  `add_node` of a card needs only `makes` (+ `prompt`); a card added without takes can't be `done` (made `draft`);
  `pick` must index `takes`. Asset nodes may name any file under the root; image sizes are filled from the header
  (assets and takes), video/audio are left to the caller. An asset's `media` may be omitted (`StudioMedia`'s
  default is `Unknown`): the server infers it from the path for every asset, so an agent that forgets it isn't
  refused. A document asset (`text`) is sized `size::DOC_W` × `DOC_H` (320 × 400).
- **Server:** the block's root is the given cwd (split: the split block's, tab: the focused block's, else `~`); a
  new block opens on Setup, else the first canvas in tab order (open, active), else Production; `studio.toml`
  appearing (init, or written by hand) moves a Setup block on, and its removal back to Setup. `studio.toml` is
  edited with `toml_edit` (comments and unknown keys kept); `canvas_create` appends to `[canvases] order`,
  `canvas_delete` removes it. Entity frontmatter is a lenient YAML subset, and updates rewrite only the keys they
  change. Without `sheet` in the frontmatter, a `sheet.<image>` in the folder is the sheet. `world/style/` is not
  a section (Style's images). `used_in`: its Entity nodes and Asset nodes of files in its folder, the cards wired
  from those, and cards whose last request (`input`) names one of its files.
- `canvas.json` keeps unknown top-level keys and nodes of unknown kinds as written; nodes missing ids get ones. Its
  rev is the file's, except that a file changed outside the server reads one above the last rev seen. Writes are
  serialised per file in the process (canvases, `studio.toml`, entity files). Placement without coordinates: in a
  frame right of its last child (frames grow to hold what is placed, output dots included), at the top level
  right of the content, but a top-level frame goes **below** the content (scenes stack, shots run right). Asset
  faces fit 320 units. **Sizes are shared** with the app's board (`silo_proto::studio::size`): `CARD_W` 340
  (landscape, square, audio), tall faces `max(round(340·√aspect), TALL_MIN_W 264)` (9:16 → 264, 4:5 → 304);
  `card_height(makes, w, aspect, prompt_lines)` = header 32 + face (audio 64) + the collapsed prompt (44 to 92:
  4 lines of 18 + 2·10) + footer 36; a card's aspect is its take's, else its `aspect_ratio`, else the effective
  format's. `PORT_GUTTER` 38 (ports left of a card), `OUTPUT_GUTTER` 37 (the output dot right of a node, with its
  hit slop), `GAP` 40, **`CARD_GAP` = `GAP + PORT_GUTTER + OUTPUT_GUTTER` = 115**: placement puts `CARD_GAP`
  between a node and a card next to it (either side), `GAP` otherwise; the first card in a frame starts at
  `frame.x + 24 + PORT_GUTTER` (its ports inside); a shot frame holds one card with its gutters (`card_width +
  PORT_GUTTER + OUTPUT_GUTTER + 48` wide). `layout` uses `CARD_GAP` across and `GAP` down when the ids include
  cards (unless `gap` is given), starts a frame's cards after `PORT_GUTTER`, moves frames with their contents and
  grows `in`. The board uses the same: cards made from a node (Make from this, ⌘D of cards: beside the
  selection) are `CARD_GAP` from it, imports and notes `GAP`; a card stored narrower (an older canvas) is drawn as
  stored. `canvas_edit`'s description tells agents not to pass `x`/`y`/`w`/`h` for cards and to use `layout`.
- **Growth pushes what follows.** A frame that grows to hold what is placed in it (an auto-placed node,
  `layout … in`) moves its siblings (same parent; the top level counts) that are in the way, with their contents
  and by the growth (gaps kept): growing right, those whose extent (a card's ports and a node's output dot
  included) starts at or right of its old right edge and overlaps it vertically; growing down, those at or below
  its old bottom overlapping it horizontally; and transitively those in the way of a moved one, so nothing new
  overlaps. Then its frame grows to hold it and the moved ones, and so on up (a shot grows → later shots move
  right → the scene grows → later scenes move down). Everything else stays. One rev for the batch.
- **`tidy { frame? }`** re-flows the frames and loose nodes under `frame` (none: the top level), depth first:
  child frames in `order` (then by position) in a row when all are shots (`GAP` apart), else in a column
  (`2·GAP`), from the frame's content corner (`PAD`, title) or the content's top left; the loose nodes as one
  block kept as arranged, before the frames when they started before them, else after; each frame then fitted
  to its contents (`PAD` around, `PORT_GUTTER`/`OUTPUT_GUTTER` included), shrinking or growing; a tidied `frame`
  that grew pushes as above. Only positions and frames' sizes change (no reordering of `order`, no wires);
  idempotent, and a tidy that moves nothing is no change (no rev). `canvas_edit`'s description ends with "after
  adding many frames and nodes, finish with `{op: "tidy"}`"; `canvas_read` starts with `Layout: N frames
  overlap; run canvas_edit {op: tidy}` when sibling frames overlap. The board's empty-canvas menu has **Tidy up**
  (the whole canvas); it applies nothing locally, and its undo is `update_node` of every node back to where it
  was (frames with their size), computed when sent (not undoable above 2,000 nodes).
- `studio.changed` goes out at once after the server's own writes (on the calling block) and debounced 150 ms from
  the watcher (FSEvents on the root on macOS; elsewhere the root plus `world/` and `canvases/` recursively):
  `studio.toml` → project; `world/**` → world; a canvas folder → project + canvas; `canvas.json` → canvas with its
  rev (gone: project); `assets/**` and a document directly in the canvas folder (`script.md`) → canvas without
  a rev (the app lists its documents and re-reads the shown ones). Reads run in the parallel lane, writes in the
  slow lane.
- `studio.import` copies through a temp file; a file already in that `assets/`, or one of the canvas's own
  documents, is answered as is (a document is pinned where it is); an absolute
  path outside the root needs owner rights (`forbidden` otherwise). `canvas_delete` falls back to deleting.
- `Gen.error_code: Option<String>` (`no_provider`, `input`, `model`, `invalid`, …) beside `error`.
  `Section.refs`: Style's reference images (every image under `world/style/`, root-relative).
- The `files.*` methods take a Studio block too (rooted at the studio root, Files' rules): the pane reads and saves
  world text with `files.read` / `files.write` (`base` conflicts) / `files.create`, and `+ Section` is
  `files.mkdir world/<name>` (no `studio.section_create`; `entity_create` also makes its section). `entity_update`'s
  `sheet` and `refs` are entity-relative as in the frontmatter; `EntityInfo` reports them root-relative.
  `entity_update {fields: {refs: [...]}}` still replaces the whole list (tables as JSON objects, written in the
  block style); `studio.entity_ref` is the one-change form the app uses, so a concurrent edit of another entry
  isn't lost. The frontmatter reader takes `- file: …` items with more `k: v` lines under them and flow tables
  `{file: …, name: …}`; one-level tables only.
- **Tools** (`silo-tools`): `Tools::studio_block(anchor, cwd, open)` and `Tools::control()`. A found block must be
  on the same root (one linked to the anchor first, then any in its session; without an anchor, in the default
  session); a new one is split off the anchor and focus returns to the anchor (a tab in its session when the split
  fails). `studio_read` and `canvas_read` open the block too (every `studio.*` call needs one). `canvas_create`
  runs `studio.init` (the folder's name, landscape) when there is no `studio.toml`, then opens the tab.

## 6. otto

### 6.1 Tools (`silo-tools`, deferred like `changes_show`)

| Tool | Input | What it does |
|---|---|---|
| `studio_open` | `path?`, `canvas?`, `world?: section or entity id`, `production?: bool` | opens or focuses the calling agent's Studio block (linked), on a canvas, the World view or the Production view |
| `studio_read` | `path?` | the project in a compact text: name, formats and defaults, then a **Production** block (each asset's sheet, approval, voice and video sheet marks and how many canvases use it; each canvas's script, shots, keyframes, video, sound, cut and out-of-date, failed and running counts; the model picks; the next steps, numbered, the user's marked), then canvases (title, episode, format, node counts, documents), world sections with each entity's name, summary, sheet and refs paths. Without Silo there is no `studio_read` (the Studio tools need a block) |
| `canvas_read` | `canvas`, `frame?` | a canvas as an outline: first its documents (path, and whether each is on the canvas as which node), then frames in order with their nodes (id, kind, title, media path, text); cards with what they make, prompt, model, options, status, their takes with the pick and its path, and **Out of date** with what changed, and their prompt's mentions with what each names and whether it is wired (`mentions @characters/mara = "Mara Venn" (wired reference from n_ab)`); wires as `n_a → n_b (first_frame)`; no pixels. Images can be looked at with `read` when the model needs to see them |
| `canvas_create` | `title`, `episode?`, `format?` | `studio.canvas_create`, then opens its tab |
| `canvas_edit` | `canvas`, `ops` | `studio.canvas_edit`; returns the new ids (and `mentions`: what the prompts' mentions wired). Cards are `gen` nodes (`makes`, `prompt`, `model?`, `options?`) and wires are edges with a port role, so the agent builds a graph of shots and then runs it; a mention in a prompt (`@characters/mara`, `@#n_3f2a9c`) wires its source and expands when the card runs, so the agent writes those rather than numbering images by hand |
| `canvas_run` | `canvas`, `nodes` | runs those cards (§6.2); an otto tool, since it generates |

World text needs no tools of its own: entities are files (`read`, `write`, `edit`), sheets are `generate_image`
with a `path` in the entity's folder. `studio_read`'s description says so.

A canvas's documents are files too (`write canvases/<name>/script.md`). `canvas_edit`'s description says a
document is pinned with `add_node` of an asset whose `path` is the `.md` file (no media needed), top-left, instead
of restating it in a note, and wired into a card's `text` port only when its text should be context. The
`studio` agent and the `series-writing`, `shot-listing` and `studio-onboarding` skills do so and tell the user
where it is. Built: the tools list documents with `files.list` on the Studio block (`studio::documents_of`);
`canvas_read` shows a document node as `document <path>`.

### 6.2 Running a card

One path makes every take, whoever asks for it: the Run button on a card, the agent's `canvas_run`, and a
`generate_*` call aimed at a canvas.

**Compiling** (otto, `otto-media`): a card and what is wired into it become one request.
- The prompt: the card's prompt, the texts wired into `text` as context (a note's text; a document's **file
  contents**, the first 8,000 characters with a note when longer; a Speech card with no prompt reads the wired
  text), and, for image and video cards with "Use the project's style" on (the default), the style line of
  `world/style.md` (the text under `## Style line`) at the end; video cards then get the **motion line** (the
  text under `## Motion line`) after it. The style line says how a frame looks; the motion line how things move
  (timing, animation style, camera), so an animated show's clips don't drift toward glossy 3D or live action
  while the stills hold the look. Image cards never get the motion line, and "Use the project's style" off
  (`options.style: false`) leaves out both. Each is read the same way: its heading at any level, matched
  case-insensitively, then the text to the next heading, its lines joined on one line, blank lines apart from
  the prompt; a missing or empty section adds nothing. The motion line is optional: new studios' `style.md`
  has no `## Motion line` (a project opts in by writing the section), and without one a run is as before.
- Inputs: wired sources become files by port: `reference` → `references`, `first_frame` / `last_frame` → the
  frame fields; `middle_frame`, `video` and `audio` need a model whose schema has a field for them (MEDIA.md §3.2:
  media fields by kind), through `generate_media`. With the model on Automatic the card picks one that takes what
  is wired; a chosen model that can't is an error on the card that names what it can't take.
- Size: the card's own `options`, else the effective format (§2), the nearest the model offers.
- **Mentions** (§3) in the card's own prompt are expanded, each token where it stands (the same token twice the
  same way twice), to its label (the entity's name, the node's title or file name: what `CardInput::label` is)
  and where its source went in the request: wired at `reference`, `Mara Venn (image N)`, N its 1-based position
  in the references as actually sent (a `generate_* {canvas}` call's own files go first and shift it); at a frame
  port, `… (the first frame)` / `(the middle frame)` / `(the last frame)`; at `video` / `audio`, `(video N)` /
  `(audio N)`; at `text`, the label alone (the text is still appended as context). A token that resolves but has
  no wire into the card is its label, with a note (`Mara Venn is mentioned but not wired.`); a wire the model's
  route drops (no field for it, a single field already filled) gives the label alone. A named reference
  (`@characters/dhritarashtra#crown-off`) reads as the entity's name and its own (`Dhritarashtra, Crown off`, not
  its file's name) with the same placement, and at `reference` with its `defines` as its job: `Dhritarashtra,
  Crown off (image 2: take only his bare head and hair, without the crown)`; without `defines`, `(image 2)`; at
  a frame, `video` or `audio` port the usual wording (no job); unwired, the names and the note (`Dhritarashtra,
  Crown off is mentioned but not wired.`). Its wire is the one from an Asset node of its file (not an Entity
  node's, which is the plain token's). Tokens that don't resolve stay as written. The card keeps its tokens; the take records the expanded prompt (the compiled one), and the
  preview shows it.
- A wired card that has never run is an error ("Run Keyframe 3 first"), unless it is run in the same request:
  `canvas_run` runs the cards it is given in dependency order. One still running is waited for.
- What marks a card out of date is what its request used: the files and texts wired in. A change to the style line,
  the motion line or an entity's text marks nothing (re-run what should follow it).
- The same compile gives the **preview** the settings panel shows (prompt, model, files, size), without running.

**Running**: the card goes `queued` → `running` (with the provider's progress, at most once a second) → `done`
with a new take in the canvas's `assets/` (named from the prompt), picked, and `inputs_used` set to its inputs'
keys; or `failed` with the provider's message, or `stopped`. Everything is written through `studio.canvas_edit`,
so every client watching the canvas sees it. otto reaches the server through its silo connection
(`otto-silo-tools`).

| Who | How |
|---|---|
| The app (Run, Stop, the preview) | otto's `studio.run_card` / `studio.stop_card` / `studio.preview_card` `{cwd, canvas, node}` (otto-proto): no session and no model turn; returns at once, the card carries the state. No provider key: the card fails with `no_provider` and the app shows the media setup card (MEDIA.md §5.2), then runs it again |
| The agent | `canvas_run {canvas, nodes}`: the same run as a background job of its session (MEDIA.md §6.5), so the results arrive as a message with their `[generated …]` lines |
| `generate_*` / `generate_media` with `canvas: {name, frame?, node?, title?, style?}` | the quick way for the agent to make a card and its first take in one call: a card is added in `frame` (or `node`, an existing card, gets a new take) with the call's prompt, model and options, wired from every input that is a node of that canvas or a world entity's file (an `Entity` node is added for those), and the result becomes its take (`n` results: `n` takes of the one card). It compiles like any card: the style line (and on video the motion line after it) is added unless `style: false`, missing size fields come from the effective format. An input that is the card's own take (an edit of it, a `markup` edit) is used as a file and makes no wire. The result line adds `Placed on canvas ep01-pilot as n_3f2a9c` |

Without Silo (otto alone, CLI) a `canvas` target saves the files to `canvases/<name>/assets/` and the result says
the canvas wasn't updated. The call still compiles as far as it goes: `generate_image` / `generate_video` get the
style line (and `generate_video` the motion line after it) unless `style: false`, and unset sizes from the format;
its prompt's mentions expand from the files on disk (an entity's name from its folder, a node's label from
`canvas.json`, a named reference's names and `defines` from the entity's frontmatter), numbered by where the
call passes the entity's sheet or ref (a reference: its own file only), or the node's output, in `references`,
`first_frame` or `last_frame` (none: the label and the not-wired note).

**Built** (otto; differences from the above, and how it runs):
- **Bridge:** `otto_tools::studio::Studio` (`block(cwd, sessions, open)`, `project`/`canvas`/`world` as the
  server's JSON, `edit(ops as JSON)`, `card(block, canvas, node) -> Card`, `sources(paths)`), implemented over the
  Silo connection in `otto-silo-tools` (`Silo.studio`) and set on `otto_media::Media` (`set_studio`) by the daemon.
  `Card` is plain data resolved on the Silo side with `silo_proto::studio`'s helpers (`inputs_of`, `output_of`,
  `input_keys` with file mtimes in **unix ms**, `format_for`), so `otto-media` needs no Silo connection; it
  depends on `silo-proto` for the mention parser only. `CardInput.entity` is the entity id of an Entity-node
  source (what `@characters/mara` matches) and `Card.mentions: [CardMention { token, label, path?, defines? }]`
  the tokens of its prompt that resolve (`Canvas::mentions_of`), for the unwired ones' labels; `path` and
  `defines` are a named reference's (`@characters/mara#coat`: its wires are those whose `CardInput.path` is that
  file and no `entity`). A wired
  document is a `CardInput` of media Text with its `path` and no `text`; `compile` reads it under `Card.root`
  (`DOCUMENT_CAP` 8,000 characters, a note when cut; unreadable: an `input` problem). Disk reads
  shared with mentions (`root_for`, `entity`, `entities`, `mention_target`, `style_line`, `motion_line`,
  `model_picks`, `canvas_file`) live there too; `WorldEntity` carries `looks: [WorldRef { id, path, name,
  defines? }]` (`WorldRef::kind()`: `look`, `video`, `audio`) and `links` (both ends from `entity`/`entities`),
  read with the same ids as the server's (`ref_ids`);
  `style_line` and `motion_line` share one reader of a `world/style.md` section. `Setup::read(root)` gathers
  `style`, `motion` and `[models]` picks for `compile`.
- **Compile:** `otto_media::studio::compile(&Media, &Card, &Setup, extra) -> Result<Compiled, Problem>` gives a
  tool name and its exact input (`generate_image`/`_video`/`_audio` with `provider` + model id, or `generate_media
  {model: handle, mode, input}`), resolved with the tools' own model/mode lookup. Ports map to the quick tools'
  fields where the built-in model lists that input (`reference images`, `first frame`, `first/last frame`), else to
  a mode's top-level media fields by name (first/start, last/end/tail, middle; lists and reference/style/subject
  names → `reference`; a lone image of a video model → `first_frame`; video and audio fields → those ports;
  masks, `file` and nested fields are never filled). A mode fits when every wired port has a field with room and
  every required media field is wired. Automatic tries the `[models]` pick, the kind's default, then each
  connected provider's built-in models (live schemas are fetched only for the first few). Audio on Automatic with
  no pick leaves the model to the provider, as `generate_audio` does. Options the route's schema lacks are left out
  with a note; size fields take the nearest enum value with a note (`Kling 3.0 Pro has no 4:5; made 9:16.`).
- **Run:** one function (`studio::run`): register the card (a cancel token, persisted in
  `<otto home>/studio-runs.json`), `queued`, wait while a wired card is queued/running (polled every 2 s), compile,
  `running` with `tool`/`input`, call the compiled tool with `path: canvases/<c>/assets/<free stem>` (no file named
  `<stem>.*` or `<stem>-<n>.*`) and `background: false` in a copy of the caller's `ToolCx` (cwd = the studio
  root, no job registry, progress forwarded to the card at most once a second), then `done` with every file as a
  `Take` (`pick` = the first new one) and `inputs_used`; or `failed`/`stopped`. The card's `job` is not set.
- **Session-less (the app):** the same call with a bare `ToolCx` (no session, attachments, approver or jobs).
  `result::result` then keeps no attachment and only saves `path`. `studio.run_card` compiles once first; only
  `no_provider` is answered there (and fails the card), anything else fails the card when it runs. A daemon start
  marks the cards listed in `studio-runs.json` `stopped` when Silo answers.
- **`canvas_run`:** all cards `queued` in one edit, then each its own session job (`launch`, MEDIA.md §6.5) in
  dependency order; a card wired from one in the set waits for it, one wired from a draft outside fails "Run X
  first". One message per card.
- **`generate_* {canvas}`** (`studio::place`): call files that are a node's output are wired from it; an entity's
  sheet gets an `Entity` node, another of its refs an `Asset` node; links, `att_` ids, other files, the card's own
  takes and `markup` go into the request without a wire (`extra`; their references come first). With `node`, the
  card's old wires are removed so it compiles to the call, except those its prompt's mentions made (the server
  removes those with their tokens). Needs a `studio.toml` above the cwd.
- otto-proto adds `media.models`, `media.model_schema` (the catalog and schemas the app's settings panel offers),
  `studio.run_card` / `stop_card` / `preview_card {models}` (`CardPreview { compiled, error, waiting, ports, fits }`)
  and `studio.mentions` (the composer's entity and canvas candidates). `no_provider` is an RPC error with
  `data: {kind, provider}`; the card carries only the message.

### 6.3 `@` world mentions

`MentionKind::Entity`: `target` is the entity id (`characters/mara`), or an entity id and one of its named
references (`characters/mara#hood-down`; `otto_tools::studio::mention_target`). The composer's `@` popup lists entities
and canvases (when the session's folder is a studio; `studio.mentions`) after agents and before models for a typed
query: `@mar` → `Mara Venn   Character`. A bare `@` lists them first, under `World` and `Canvases` headings (the
first 4 of each; otto keeps room for canvases however many entities there are), then agents and paths. otto
adds to the model's note:

```
Mentioned world entities:
- Mara Venn (character): world/characters/mara/mara.md; sheet world/characters/mara/sheet.png;
  refs world/characters/mara/refs/turnaround.png, …. Pass the sheet (and refs that fit) as references when
  generating her; read her file for details.
- Mara Venn, Hood down (character reference characters/mara#hood-down): world/characters/mara/refs/hood-down.png;
  defines her hair once the hood is down. Pass this file as a reference when generating them (a card prompt's
  @characters/mara#hood-down wires it and states its job); entity file world/characters/mara/mara.md.
```

A query with a `#` after an entity lists its references: `studio.mentions` with `characters/mara#text`, or
`<folder or name>#text` when no canvas has that name (a canvas wins), `otto_tools::studio::looks`, filtered by
what follows `#` (name or id, a prefix first), in file order when empty: `Hood down   Look · Mara Venn`
(`Video ·`, `Audio ·` by the file), target `characters/mara#hood-down`, `entity: true`. In the composer a picked
entity, like a canvas, gets no trailing space and `#` typed right after its chip reopens it as that query
("Finding references…" until otto answers); a pick makes the chip `@characters/mara#hood-down`, reading
`Mara Venn · Hood down` with its media's icon, in the composer and in sent messages (a reloaded message asks
`characters/mara#` once for its entity's references, whose details name the entity too). A plain query also
lists the references whose name or id contains it (case aside; a substring, not fuzzy, to keep noise down), each
right after its entity's row when that entity is listed, else after the entities, within the same `limit`:
`crown` → `Kuru crown   Prop`, then `Crown off   Look · Dhritarashtra`. The composer names those rows whole and
places them under their entity's row.

Canvas nodes are mentioned the same way (`MentionKind::Canvas`, target `ep01-pilot#n_3f2a9c`, or a canvas alone):
the note gives each node's kind, path and prompt, so "Ask agent" selections arrive with everything needed.

From the popup, a picked canvas is a chip with no trailing space (like a folder): typing `#` right after it
makes it the query `@ep01-pilot#` again (any other character leaves the chip), and the popup lists that canvas's
nodes (`studio.mentions` with query `canvas#text`, `otto_tools::studio::nodes`), filtered by what follows `#`:
frames by title (`Pass 3A · The quay fight   Scene`), cards by title else their prompt's first line
(`S2.3 key   Image card · S2.3`), notes by their first line, documents and other assets by file name; the detail
names the smallest titled frame holding the node. Untitled frames, empty notes and drafts and entity nodes are left
out. Picking one makes the node mention `ep01-pilot#n_3f2a9c`; its chip reads the node's title (an "Ask agent"
node chip too, once otto has named the canvas's nodes), its canvas's title until then.

### 6.4 The `studio` agent

A second entry in `BUILTINS` (`otto-core/src/agents.rs`):

```
name: studio
description: Co-creates images, video, audio and series in a Studio: world, characters, scripts, shots, sound.
tools: studio_*, canvas_*, generate_*, list_media_models, media_model_schema, files and search, shell, job,
       web_fetch, web_search, ask_user, update_todos, skill, subagent
loadable: everything else
```

What a studio needs is shown from the first turn: the Studio and canvas tools (deferred for other agents) and the
media tools need no `load_tools`. The rest (browser, terminal, Files and Changes views, otto settings, MCP) stays
granted behind `load_tools`.

Its prompt (bundled, `otto-core/agents/studio.md`, `include_str!`, listed under `[extra-inputs]` in `corgi.toml`)
sets how it works:
- Start from the world: read `studio_read` first, its Production block first, and say in a line where the
  production stands when a conversation starts or the user asks what's next; keep `world/` the source of truth and
  update it when the story invents something (a new place, a scar, a rule).
- An approved sheet is final: never regenerated or overwritten (an alternative goes to a new file and the user is
  asked). Before building a scene's cards it names any asset in it with no sheet or an unapproved one and offers to
  make it or asks for approval; told "approve it", it writes `approved: <date -u +%Y-%m-%dT%H:%M:%SZ>` into the
  frontmatter itself. A piece of work ends with the next step.
- Named references and links: the prompt says what the table form of `refs` is, that a reference's id is its
  file's name without the extension (so it names the file for what it is, `refs/crown-off.png`), and that the
  agent writes the table itself when it makes a reference (plain files; `studio.entity_ref` is the app's); `studio_read` lists each
  entity's references by id with name and `defines` (` · refs face world/…/face.png; crown-off "Crown off"
  world/…/crown-off.png defines "…"`, the name shown only when it isn't the file's stem) and its links (` · links
  props/kuru-crown`); `canvas_read` outlines a reference mention with its file and job (`@characters/mara#coat =
  "Mara Venn, Coat" (world/…/coat.png, defines "the coat"; wired reference from n_cd)`). The skills
  (`character-design`, `location-design`, `prop-design`, `shot-listing`, `continuity`, `image-prompting`,
  `video-prompting`) make states, outfits, angles and views named references, link a prop to its owner, and name
  the reference a shot needs in its note (`Who: characters/dhritarashtra#crown-off`) and its card's mention.
- Consistency first: always wire or pass the entity sheets (and refs that fit) as references; reuse a shot's last
  frame as the next one's first frame within a scene; the style line of `style.md` goes on every shot (a card's
  run adds it; a file made with `path` writes it in), and its motion line, when the project has one, on every
  video shot after it (a video card's run adds it); the medium line alone on sheets.
- Plan, then make: an episode becomes a script, then scene and shot frames on its canvas with notes, then
  keyframes, then video, then sound, then the cut. Say what it is about to make in a line; it doesn't wait for
  permission unless the user asked it to.
- Run long generations in the background and keep working (more shots, the next scene's keyframes).
- Pick models by the job (`list_media_models`), say why in a line, and remember the user's picks in `studio.toml`
  under `[models]` (`video = "kling-3.0-pro"`) for the next time.

**Bundled skills** (`otto-core/skills/<name>/SKILL.md`, embedded like the prompt; discovered first, so user and
project skills of the same name replace them; visible to every agent, so `build` can use them too). The prompting
skills are written from the vendors' current guides and checked against the catalog here; model ids move fast, so
they tell the agent to read `list_media_models` rather than trust a list. `acting-direction`, `action-choreography`
and `animation-direction` are written from published production workflows for long multi-shot generations (timed
shots with hard cuts in one clip, a clip re-shot from new angles as a motion reference, extend), in our own words:

| Skill | For |
|---|---|
| `studio-onboarding` | the first conversation: premise, genre, tone, audience, references, format, length → `overview.md`, `style.md`, `studio.toml` |
| `worldbuilding` | rules, history, factions, places; what to write down and where |
| `character-design` | a character's file, then the asset pipeline: a three-view sheet in one image, combining the face of one option with the outfit of another, the fixing pass (edits, or the user painting over the file), state references, voice casting, a 360° video sheet with a spoken line |
| `prop-design` | props, vehicles, machines: the file with counted parts, a two-view sheet, interior views cut from one video of held stills, a video sheet that shows the moving parts work, mechanism clips, a placement image |
| `location-design` | place files and establishing/plate images, times of day, weather variants |
| `style-guide` | writing `style.md`: palette, lenses, lighting, texture, era; a style frame set |
| `image-prompting` | stills: keyframes, style frames, sheets, plates; prompt structure, references and edits, per-model practice |
| `series-writing` | series arc, episode outlines, scripts with scene headings and beats; a five-question check of whether a scene works |
| `shot-listing` | from a script to scene and shot frames: framing, movement, duration, the format's composition rules (vertical framing differs) |
| `video-prompting` | per-model prompt shapes (Kling, Seedance, Veo-style, Wan, Grok Imagine), image-to-video vs reference-to-video, motion words that work |
| `dialogue-scenes` | scenes of talk: every line recorded first and measured, the scene split into parts (one multi-shot generation of one to three lines each), an input map giving each image, clip and recording one job, beats timed to the words, a live listener, counted constraints, continuing from a clip, cutting from passages of several takes |
| `acting-direction` | performance in dialogue, reaction and close-up shots: the scene's shared direction, each character's motive, goal, obstacle and tactic, written as a performance block (verbs aimed at the partner, a task for the eyes, beats keyed to the lines); takes and picking passages |
| `action-choreography` | fights, chases, stunts: timed beats with reactions, body geometry in place of technique names, positive locks in place of prohibitions, the place's sides and a blocking image, a multi-shot **pass** (one generation, several timed shots), then re-coverage and an extend |
| `animation-direction` | shows that are not live action: the motion line of `style.md` (appended to video cards, §6.2), key poses and timing, drawn effects, impact inserts as their own shots, keeping video from drifting to glossy 3D or live action |
| `continuity` | last frame → first frame, eyelines, screen direction, wardrobe, time of day across shots and episodes |
| `sound-design` | dialogue lines per character voice, SFX, music cues, levels |
| `assembly` | the cut with `ffmpeg` through `shell`: shots in order, crossfades, dialogue + music + SFX mix, subtitles, output to `out/` at the canvas's format |

### 6.5 Agent block in a studio

- A new agent block split off a Studio uses the `studio` agent and is linked both ways (`AgentLink`).
- A `[generated …]` result placed on a canvas shows a small `On canvas · Ep 1 Pilot ›` link under its tile; it
  focuses the node.

## 7. The journey

1. Open Silo in a new folder → New Studio → Setup: name and format → **Start with the agent**.
2. Onboarding interview (`studio-onboarding`): premise, tone, references → `overview.md`, `style.md`, a style frame.
3. World: characters and places, each with a file and a sheet; the user curates in the World view, sets sheets,
   asks for changes from a card.
4. Episode 1: `canvas_create {episode: 1}`, `script.md`, scene and shot frames with notes.
5. Keyframes per shot with the entities' sheets as references, then video from them, in the background; the user
   reviews on the canvas, selects, asks for variants.
6. Dialogue with each character's voice, SFX, music; the cut in `out/`.
7. Episode 2 reuses the world; a remade sheet marks the shots wired from it out of date; a vertical cut of the same
   episode is a canvas with `format: vertical` that reuses the keyframes.

## 8. Tests

- Server (`silo-server/tests/studio.rs`, a temp folder each): `init` layout and never overwriting; project parse
  with presets and canvas order; format resolution shot > canvas > project; `canvas_edit` every op, auto
  placement, `layout`, `base_rev` behind, a bad id failing one op only; concurrent edits serialised; atomic writes;
  `studio.changed` after a server write and after an outside file edit; entities with and without frontmatter,
  custom sections, `used_in`; paths outside the root refused; chunked `studio.asset`.
- otto: compiling a card (prompt parts, style line on and off, the motion line on video only and after the style
  line, inputs by port and their order, size from the
  format only where unset, a model that can't take an input, a draft upstream); a run adds a take, picks it and
  sets `inputs_used`; `canvas_run` in dependency order; a generation with `canvas` makes a wired card with its
  take in `assets/`; progress throttled onto the card; `run_card` without a provider fails with `no_provider`; no silo connection
  degrades as §6.2 says; entity and canvas mention notes; the `studio` built-in and bundled skills are listed and
  can be overridden by name.
- App: canvas view model against fixtures (hit testing, snapping, frame adoption, reading order, fit), the
  header's tab order and format chip text, World sections from `Project`, every shared `BlockPane` match covers
  Studio (`Kind::ALL`).

## 9. Order of work

Subagents in parallel once the model is in (one commit the others build on):

1. **Protocol + server** (`silo-proto`, `silo-server`, `silo-tools`): §3 types, then project/world/canvas reads,
   `init`, `canvas_edit`, the watcher and events, then the tools of §6.1.
2. **otto** (`otto-llm`, `otto-proto`, `otto-core`, `otto-media`, `otto-silo-tools`): the `canvas` field and
   placement, mention kinds and notes, the `studio` agent and bundled skills (their content is its own task:
   curated and tested against real generations).
3. **App** (`silo-app`): the block and core matches, header and tabs, Setup, World view, then the canvas (view
   model first, then drawing, media nodes reusing the agent block's tiles and viewers, editing, edges, drop,
   Ask agent).

Milestones:
- **S1 Studio:** setup, `studio.toml`, formats and the chip, World view with entities and editing, `studio_*` tools.
- **S2 Agent:** the `studio` agent, bundled skills, `@` entity and canvas mentions, onboarding; canvases as tabs
  with the whole server model (§3, §5); running cards (§6.2) from the agent and from the app.
- **S3 Canvas:** the board of §4.2: pan and zoom, cards with their prompt and Run, ports and wires, drag to
  create, the canvas prompt, takes, settings, frames, drop, minimap.
- **S4 Series:** episode ordering, scene/shot frames as storyboard, assembly into `out/`.

S1 to S3 are built together, end to end; the board reuses the tiles the first app pass makes.

## 10. Open questions

- §6.2's path from a media tool (and its background job) to `studio.canvas_edit` through otto's silo connection
  is designed from the docs, not yet proven in code; the otto work settles it.
- Audio wired into a video card: ved-web lays voice lines on a timeline and sends a guide video so the model
  follows the timing; here the first pass sends the audio files as the model's audio inputs and leaves timing to
  the prompt.
- Assembly with `ffmpeg` joins clips; trims, audio timed to picture and lip sync are not solved by it.
- A timeline view of an episode (shots as clips with trims) beyond the canvas's reading order: after S4, if the
  ffmpeg assembly isn't enough.
- Several studios in one folder (`studio.toml` in subfolders): one per root for now.
- Sharing a world between projects (spin-offs): a `world = "../other/world"` key, later.
