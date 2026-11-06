# Media generation (images first, then video and audio)

Status: M1 (images) built; M2 (video) and M3 (audio) built in the backend (`otto-media`), their agent-block UI (§6.4) in progress.

## 1. Idea

The agent makes media the way it does anything else: it calls a tool. `generate_image` (later
`generate_video`, `generate_audio`) sends the prompt to a media provider, stores what comes back with the
session, and returns it. The agent block shows the result in the transcript, full width, and opens it in a
viewer (zoom, pan, copy, save).

A media provider needs an API key. When none is set up, the tool doesn't fail: it asks the user, and the agent
block answers that question with a setup card right in the chat (provider rows, "Add key" opens the usual key
dialog). Once a key is saved the card answers for the user and the same call goes on. No re-prompting.

## 2. Providers

| Id | Name | Kinds (M1 built: image) | Default image model | API |
|---|---|---|---|---|
| `openai` | OpenAI | image, audio (speech) | `gpt-image-2.6-flare` (GPT Image 2.6 Flare; also 2.6 Sunburst, 2.5 Flare/Sunburst, `gpt-image-2`) | `POST /v1/images/generations`, `/v1/images/edits` (references, `markup`), `b64_json`; 2.6: `complexity`, `output_format: svg|gif` (generations only) with `frame_count`/`frame_duration_ms` |
| `google` | Gemini | image, video | `gemini-nano-banana-2.1` | Interactions API `POST /v1beta/interactions`, `response_format: {type: image, aspect_ratio, image_size}` |
| `xai` | xAI | image, video | `grok-imagine-image-2.0` | `POST /v1/images/generations` and `/v1/images/edits` (OpenAI shape), `response_format: b64_json` |
| `fal` | fal | image, video | `blackforestlabs/flux-3/text-to-image` (FLUX 3; references go to `…/flux-3/edit-image`) | `POST https://fal.run/<model>` with `Authorization: Key …`, `images: [{url, width, height, content_type}]`; references as data URIs in `image_urls` |
| `higgsfield` | Higgsfield | image, video | `higgsfield-ai/soul/v2/standard` (Soul 2; it takes no references, so calls with references go to `xai/grok-imagine-image-2.0`) | `api.higgsfield.ai`, one key (ID + secret) for 50+ models (Soul, Seedream, Recraft, Ideogram; video: Kling, Seedance, Veo, Wan, MiniMax…); asynchronous: `POST /<model>` with `Authorization: Key id:secret` and an `Idempotency-Key`, poll `status_url` (2 s growing ×1.5 to 10 s; `queued`, `in_progress`, then `completed`, `failed`, `nsfw` or `canceled`; a cancel posts `cancel_url`), download (files kept 7 days). References are uploaded first (`POST /files/generate-upload-url`, `PUT`, then their `public_url`) |
| `elevenlabs` | ElevenLabs | audio (speech, sound effects, music) | – | `api.elevenlabs.io`, `xi-api-key` |

Model ids move fast (gpt-image-1 was shut down in Oct 2026, Imagen too): each provider has a default and a
short list of known models; `model` on the call and the per-kind default (§4) take any id.

### 2.1 Keys

Media keys are separate from chat sign-ins, so a ChatGPT subscription sign-in and an OpenAI API key for images
can both exist. A provider's key is the first of:

1. its media key: credential `media:<id>` in the same credential store (`auth.json`), set with `media.set_key`;
2. the chat provider's credential of the same id when that is an API key (`openai`, `google`, `xai`);
3. its environment variable (`OPENAI_API_KEY`, `GEMINI_API_KEY`/`GOOGLE_API_KEY`, `XAI_API_KEY`, `FAL_KEY`,
   `HF_CREDENTIALS` (`id:secret`) or `HF_API_KEY_ID` + `HF_API_KEY_SECRET`, `ELEVENLABS_API_KEY`).

A two-part key (Higgsfield's ID and secret) is stored as one string `id:secret` (`media.set_key` refuses one
without both parts); the key dialog says so. Higgsfield sends it as is: `Authorization: Key id:secret`.

`MediaProviderInfo.source` says which one (`media`, `chat`, `env`); a subscription sign-in never counts.

## 3. Tool: `generate_image`

Built-in (`otto-media`; `Media::tools()` gives it, the other `generate_*`, `list_media_models`, `media_model_schema` and
`generate_media`), added to every session through `Runtime::add_tools`, eager. The quick path: a prompt and simple
references/frames; anything beyond that goes through §3.2.

```
generate_image {
  prompt: string,                       // what to make; for edits, what to change
  references?: [string],                // images to edit or follow: project paths or attachment ids (att_…)
  aspect_ratio?: "1:1"|"16:9"|"9:16"|"4:3"|"3:4"|"3:2"|"2:3",
  quality?: "low"|"medium"|"high",
  transparent?: bool,                   // transparent background (providers that can; others say so); png/webp only
  n?: 1..=4,                            // default 1
  complexity?: "low"|"medium"|"high",   // GPT Image 2.6: how much it plans first (API default high)
  format?: "png"|"jpeg"|"webp"|"svg"|"gif", // svg/gif: GPT Image 2.6, n 1 (svg), no references
  frame_count?: 2..=64, frame_duration_ms?: 20..=2000,  // gif only (API defaults 9, 200)
  markup?: string,                      // transparent PNG annotation layer over the first reference (GPT Image 2.5/2.6)
  path?: string,                        // also save into the project here (n > 1: name-1.png, name-2.png, …)
  provider?: string, model?: string     // else the image default, else the first connected provider
}
```

- Effects: `network`, `write` (it may write `path`); not `exclusive` (generations can run in parallel).
- Each image is stored as a session attachment (`otto_tools::attachments::put`), named from the prompt
  (`sunset-over-dunes.png`), so `read` (with `attachment_id`) and `copy_attachment` work on it as they do on
  the user's files.
- Result for the model: a text part, plus each image downscaled to 768 px as an image part when the model has
  vision (so it can check its work):

  ```
  Generated 1 image with OpenAI gpt-image-2.5-flare.
  [generated att_3f2a9c1b: sunset-over-dunes.png, image/png, 1.2 MB, 1024×1024 — copy_attachment can save it into the project]
  Saved to assets/sunset.png
  ```

  The `[generated …]` line is the contract the app parses (it survives reloads, where tool metadata does not):
  `[generated <id>: <name>, <mime>, <size>, <w>×<h> — …]`; `, <w>×<h>` is absent for kinds without one.
- Metadata (live only): `{provider, model, media: [{id, name, mime, size, width, height, path?}], revised_prompt?}`.
- Progress: `tool.progress` message `Generating with OpenAI…` while it waits.
- Errors are tool errors with the provider's message (`content policy`, `rate limited`, …), never a key.
- What a provider can't do is a line of the result (`Gemini can't make transparent backgrounds.`), not an error.
  `quality: high` means each provider's larger size where it has no quality setting (Gemini `2K`, xAI `2k`,
  Soul `1080p`). Providers that make one image a request (Gemini, fal, Higgsfield) get `n` requests at once.
- `path` replaces an existing file; a different raster extension than the result's (`.jpg` for a PNG) converts it;
  an SVG is saved only at a `.svg` path and a GIF at a `.gif` path keeps its bytes (animation).
- `complexity`, `format`, `markup` on a provider or model without them: a result line (`complexity is not supported
  by Gemini; ignored.`). Contradictions are input errors: svg with `n > 1`, svg/gif with references, frames without
  gif, markup without references or with `transparent`, transparent with jpeg/svg/gif.
- `markup` must be a PNG with transparent pixels the size of the first reference; it is scaled with it.
- The model sees no SVG; a GIF as its first frame. The line's size: an SVG's `width`/`height` or `viewBox`.

### 3.0 Choosing a model: `list_media_models`

```
list_media_models { kind?: image|video|audio, query?: string, input?: image|video|audio, provider?: string, refresh?: bool }
```

Read-only, eager. One line per model, grouped by kind and provider (with whether it is connected): handle (what
the tools take), name, what it is good at, the built-in inputs/outputs, its modes with the media fields each takes
(§3.2: `reference-to-video (audio_urls ≤10?, image_urls ≤30?, video_urls ≤10?)`), price when known, the default.
`query`: every word in the handle, name, ids, description or modes; `input`: models with a media field of that
kind. Built-in models first, then the live catalog's families (25 per provider and kind, 50 when filtered, then
`… N more`). Audio adds each provider's voices (ElevenLabs' live list, cached an hour per key). It ends pointing at
the quick tools and at `media_model_schema` / `generate_media`.

How the agent picks (in the tools' descriptions):
- the user named a model or provider ("make it with Kling", "use Nano Banana Pro") → use it, `list_media_models`
  first when the id isn't obvious;
- the choice matters (video, cost, a specific style) and the user didn't say → `list_media_models`, then either
  pick and say why in one line, or `ask_user` with 2–4 options when they differ a lot in price or look;
- otherwise the kind's default.

### 3.0.1 `@` model mentions

The composer's `@` popup (agents, files) also lists media models once a query is typed:
`@kli` → `Kling 3.0 Pro   Higgsfield · Video`. Picking one makes a chip `@kling-3.0-pro`, sent as
`InputPart::Mention {kind: model, target: "kling-3.0-pro"}`.

- Handles: each model's `handle` (`MediaModel.handle`): its name slugged (`Nano Banana 2.1` → `nano-banana-2.1`,
  `GPT Image 2.6 Flare` → `gpt-image-2.6-flare`); the same name at two providers gets the provider in front for
  every provider but the model's maker/first in list order (`higgsfield-grok-imagine-2.0`). Stable, unique,
  lowercase `[a-z0-9.-]`.
- Popup: models after agents and before files, at most 6, fuzzy on handle, name and provider; a row shows the
  kind's icon (image/video/audio), the name, and `Provider · Kind` muted, plus `Not connected` when its
  provider has no key (still pickable: the setup card asks when it runs). Not listed for an empty query.
- Handles cover the live catalogs' families too (§3.2), after the built-in ones so those never change.
- otto resolves a handle through the media catalog (`missing` when unknown, drawn struck through) and adds to the
  model's note: `Mentioned models: Kling 3.0 Pro (video) → generate_media {model: "kling-3.0-pro"}. Use them for
  the matching generation; don't ask which model. Call media_model_schema first if the request needs more than a
  prompt.` The quick tools take handles too (`model` names the provider).
- The note wins over §3.0's picking rules; a mention of an image model in a video request is the model's
  call (it may say it doesn't fit).

### 3.2 Any model: schemas on demand

The quick tools take one input set; real models take far more (Seedance 2.5: text, image, reference with images +
videos + audio, video edit, video extend; Genjutsu: motion transfer, object swap, restyle from a video and 1–8
images; Kling 4.0 video edit). Instead of a tool per model, the model reads a model's schema and calls it.

- **Live catalogs** (`MediaProvider::endpoints`): Higgsfield `GET /models?include_schema=true` (with the key:
  `{items: [{slug, title, description, operation_type, output_type, base_credits, input_schema}]}`); fal
  `GET api.fal.ai/v1/models?category=<c>&status=active&limit=40` for text/image→image, text/image/video→video,
  text→audio/speech (public; no schemas, so `MediaProvider::endpoint` reads one on demand: `?endpoint_id=<id>&expand=openapi-3.0`,
  the input schema at `paths["/<id>"].post.requestBody`, `$ref`s inlined). Kept in memory and in
  `<state>/media-catalog/<provider>.json` (`{fetched_at, endpoints}`), fetched again after 6 h (`refresh` forces;
  a failed fetch keeps the old copy and waits a minute); the daemon refreshes stale ones at start. Without a key
  or offline: the built-in models only.
- **Families**: endpoints group by the provider's group (fal `group.key`) or their id without its mode segment
  (`bytedance/seedance-2.5/{text-to-video,image-to-video,…}` → one model, modes named by what their ids don't
  share). A built-in model whose id is one of a family's endpoints is that family (its handle stays).
- **Schemas** are the provider's, trimmed: no `x-*`/`_*`/`title`/`examples`, `anyOf [X, null]` → `X`. Media
  fields: `format: uri` strings or arrays of them (also `…_url(s)` names, fal's `ui.field`); image/video/audio by
  their content type or name. Built-in models without a live catalog (OpenAI, Gemini, xAI, ElevenLabs, or no
  catalog) take their quick tool's schema without `provider/model/path/background`, the provider's options only.

```
media_model_schema { model: string, mode?: string }     // read-only
generate_media { model: string, mode?: string, input: {…}, path?: string, background?: bool }
```

- `model`: a handle, a model or endpoint id, optionally `provider/…`; connected providers first; an unknown
  `…/…` id is looked up where the provider can (fal). `mode`: its name, a unique part (`motion`), or the endpoint id
  in `model`; with several and none named, the first whose schema takes the input (every field known).
- `media_model_schema`: per mode, the endpoint, price, `Media fields (project paths, att_ ids or https links):
  image_urls 1–8 (image), video_url (video)` and the schema (compact JSON); all modes (up to 6) when none is named.
- `generate_media`: provider key first (no key: the setup question of §3.1 for the model's kind; then its catalog
  is fetched and the model resolved again). The input is validated (`jsonschema`, formats off): `Seedance 2.5
  video-edit input: "video_url" is a required property. media_model_schema {…} shows its fields`. Media fields
  holding a project path or `att_…` (any session attachment: uploads, earlier results) are read (type checked
  against the field: `video_url: x.mp3 is audio/mpeg; this field takes a video`; images prepared as references);
  links pass as they are. In the run, each file is uploaded once (`MediaProvider::upload`: Higgsfield
  `POST /files/generate-upload-url` + `PUT` for images, videos and audio alike; fal
  `POST rest.fal.ai/storage/upload/initiate?storage_type=fal-cdn-v3 {content_type, file_name}` → `{upload_url,
  file_url}` + `PUT`; others a data URI), then `MediaProvider::generate` submits (Higgsfield `POST /<slug>`, fal the
  queue) and reads the result generically: every `url`/`…_url(s)` link outside the request echo and previews whose
  content type (declared or downloaded) is image, video or audio. Stored as §3/§6.3 (`[generated …]`, `path`,
  images shown to a vision model). Background (§6.5) by default for video and music endpoints; jobs read
  `video with Higgsfield Seedance 2.5 · reference-to-video`. A built-in model without modes runs through its
  quick tool.
- Chaining: each result's `att_` id is an input of the next call (image → image-to-video → motion transfer).
- Agent block: the line is `Generating video · Seedance 2.5 · reference` (kind and name from the model in
  `media.status`, else the result, else the model/mode words), its files and placeholders placed like the quick
  tools'; the setup card shows for its question by the kind the question names.

### 3.1 No provider set up

When no provider (or the requested one) has a key, the tool asks through `Approver::ask`:

- question: `Image generation needs a provider. Connect one to continue.`
- options: the display names of the providers that can make this kind, then `Not now`
  (`["OpenAI", "Gemini", "xAI", "Higgsfield", "fal", "Not now"]`).

The answer is a provider (by name or id, any case): the tool looks the key up again and goes on with it; if it
still has none it fails with `No API key for <Name>`. `Not now`, any other answer or no answer (no client):
the tool fails with `No image provider is connected. Connect one in Otto settings → Providers.` The answer is
never a key: clients save keys with `media.set_key` and then answer with the provider.

The agent block recognises these questions by the asking call's tool name (`generate_*`) and shows the setup
card (§5.2) instead of the generic question card. Other clients (CLI) show the plain question.

## 4. Wire (otto-proto, additive)

| Method | Params | Result |
|---|---|---|
| `media.status` | `{}` | `MediaStatus {providers: [MediaProviderInfo], defaults: {kind: MediaChoice}}` |
| `media.set_key` | `provider, api_key?: string` (absent or empty removes the media key) | `MediaProviderInfo` |
| `media.set_default` | `kind, provider?: string, model?: string` (no provider: automatic) | `MediaStatus` |
| `attachment.get` | `session_id, attachment_id, offset?: u64 = 0, length?: u64` (≤ 4 MiB a call) | `AttachmentData {id, name, mime, size, offset, data_base64}` |

```
MediaKind = image | video | audio
MediaProviderInfo {id, name, kinds: [MediaKind], connected, source?: media|chat|env, models: [MediaModel]}
MediaModel {id, kind, name, handle, default: bool}   // handle: its `@` mention (§3.0.1)
MediaChoice {provider?: string, model?: string}
```

`media.set_key`'s key is redacted in `Debug` like `auth.login`'s. Defaults live in `<state>/media.json`.
The CLI has `otto-silo media status` and `otto-silo media key <provider> [--remove]` (the key typed without echo or
piped), writing the same credential store.
`attachment.get` reads the session's attachments (user files and generated media alike); bigger files are read
in chunks (`offset` + `length`, the reply's `size` is the whole file).

## 5. Agent block

### 5.1 Media in the transcript

- A `generate_image` call reads `Generating image` / `Generated image` with the prompt as its subject, like
  other tools.
- Its images are a row of their own right after the tool's group (`Row::Media`), so a folded group never hides
  them: thumbnails at most 320 pt tall (one image: its aspect ratio, up to the pane's width; several: a wrapping
  grid), rounded, a shimmer while bytes load. Bytes come from `attachment.get` (chunked), are cached per
  `session/attachment` in memory, and decoded off the main thread.
- While the call runs: one placeholder tile per requested image (`n`, at the requested aspect ratio) with a
  shimmer and the elapsed time.
- Hover: the name and size; click opens the viewer.

### 5.2 Setup card (no provider)

Replaces the question card for `generate_*` questions:

```
 ╭──────────────────────────────────────────────────────────╮
 │ ◎  Connect an image provider                              │
 │    Images are made with your own API key.                 │
 │                                                           │
 │  [logo] OpenAI       GPT Image 2.5            [Add key]   │
 │  [logo] Gemini       Nano Banana 2.1          [Add key]   │
 │  [logo] xAI          Grok Imagine             [Add key]   │
 │  [logo] Higgsfield   Soul, Seedream and more  [Add key]   │
 │  [logo] fal          FLUX and more            [Add key]   │
 │                                                [Not now]  │
 ╰──────────────────────────────────────────────────────────╯
```

`Add key` opens the existing key dialog (`signin.rs`) for that provider, with its hint and placeholder; saving
calls `media.set_key`, then answers the question with the provider's name. `Not now` and Esc answer `Not now`.
A sub-agent's question shows the same card with its `Sub-agent · title` label.

### 5.3 Viewer

An overlay over the whole agent pane (composer included): a near-black backdrop, opaque once open (it blurs
what's behind while it fades), the image fit to the pane.

- Zoom: pinch, ⌘-scroll or scroll, `+`/`-`, around the pointer; 10 %–800 %. `0` fits, `1` is 100 %.
  Double-click toggles fit / 100 % at the pointer.
- Pan: drag, or scroll when zoomed in.
- ←/→ or `[`/`]`: the previous/next image of the session. Esc or a click on the backdrop closes.
- Toolbar (top right): zoom %, Fit, 1:1, Copy (image to the clipboard), Save… (save panel), Close.
- Footer: name, `1024×1024 · PNG · 1.2 MB`, provider and model when known, the prompt (one line, full in a
  tooltip).

### 5.4 Providers settings

The Providers view gets a `Media` section below the chat providers: one row per media provider with its
kinds (`Images`), `Connected · API key` / `Uses your OpenAI key` / `From environment`, and `Add key` /
`Remove`.

## 6. Video (M2) and audio (M3)

### 6.1 `generate_video`

```
generate_video {
  prompt: string,
  first_frame?: string, last_frame?: string,   // images: project paths or att_ ids (image-to-video)
  references?: [string],                       // style/character images, where the model takes them
  duration?: number,                           // seconds; each model's nearest allowed value (the result says)
  aspect_ratio?: "16:9"|"9:16"|"1:1",  resolution?: "480p"|"720p"|"1080p",
  sound?: bool,                                // with audio, where the model can
  path?: string, provider?: string, model?: string
}
```

| Provider | Default video model | API |
|---|---|---|
| `higgsfield` | `kling-video/v3.0/pro/text-to-video` (Kling 3.0 Pro; also Seedance 2.5, Wan 3.0, MiniMax H3) | the image flow's submit → poll → download (`Higgsfield::wait`); frames switch to the model's `…/image-to-video` (`image_url`, `last_image_url`/`end_image_url`, uploaded), references to `…/reference-to-video` (`image_urls`) |
| `google` | `gemini-omni-1.1-flash` (Gemini Omni Flash; Veo 3.1 preview was shut down Oct 22 2026) | `POST /interactions` with `background: true`, `response_format: {type: video, aspect_ratio, resolution}`; poll `GET /interactions/{id}`, cancel `POST …/cancel`; the video is base64 in the `model_output` step. No length setting: it goes in the prompt |
| `xai` | `grok-imagine-video-1.5` (+ `-lite`) | `POST /v1/videos/generations` → `request_id`, poll `GET /v1/videos/{id}` until `done` (`progress` %), download `video.url`; frames `image`/`last_frame`, `reference_images`; no cancel endpoint |
| `fal` | `kling/v4/text-to-video` (Kling 4.0; frames through `kling/v4/image-to-video`) | queue API: `POST https://queue.fal.run/<model>`, poll `status_url` (queue position), read `response_url`, cancel `PUT cancel_url` |

- Polling (`poll::wait`, shared): every 2 s growing to 10 s, 15 min at most; `tool.progress` `Generating with Higgsfield… 40 s` (with the provider's
  percent or queue position when it reports one); cancel cancels the provider request where it can.
- Effects, setup question (`Video generation needs a provider. …`, options: video providers + `Not now`),
  storage and `path` as for images. The model gets no video parts: its result is the text line.
- Result line: `[generated att_…: fox-running.mp4, video/mp4, 4.2 MB, 1280×720, 8 s — …]` (dimensions and
  duration read from the MP4's `tkhd`/`mvhd` boxes; each left out when unreadable).

### 6.2 `generate_audio`

```
generate_audio {
  type: "speech"|"sound"|"music",
  text: string,                 // what to say, or the sound/music prompt
  voice?: string,               // speech: a voice id or name (list_media_models lists voices)
  duration?: number,            // sound/music: seconds
  path?: string, provider?: string, model?: string
}
```

| Provider | Speech | Sound effects | Music |
|---|---|---|---|
| `elevenlabs` (default) | `POST /v1/text-to-speech/{voice_id}` (`eleven_v4`; default voice George `JBFqnCBsd6RMkjVDRZzb`) | `POST /v1/sound-generation` (`eleven_text_to_sound_v3`, 0.5–30 s) | `POST /v1/music` (`music_v2_5`, `music_length_ms` 3 s–10 min) |
| `openai` | `POST /v1/audio/speech` (`gpt-4o-mini-tts`, voice `marin`; shuts down Jan 6 2027, its successor is Realtime-only) | – | – |

- `xi-api-key` header; MP3 (`audio/mpeg`) out.
- Voices: `list_media_models {kind: audio}` lists ElevenLabs' voices (`GET /v2/voices?page_size=100`, cached an hour per key; George alone without a key) with
  name, id and a one-line description, and OpenAI's built-in voices.
- Result line: `[generated att_…: welcome-message.mp3, audio/mpeg, 210 KB, 12 s — …]` (duration from the
  MP3 frames when readable).

### 6.3 Result line, all kinds

`[generated <id>: <name>, <mime>, <size>[, <w>×<h>][, <seconds> s] — copy_attachment can save it into the project]`

### 6.4 Agent block

- **Video row**: like an image tile (aspect ratio from the line, 320 pt max): the poster frame (first frame,
  `AVAssetImageGenerator`, off the main thread), a play badge and the duration. While running: a placeholder
  at the requested aspect ratio with the elapsed time and the provider's progress text.
- **Video viewer**: the image viewer's overlay; the video is drawn by gpui (no native view): an `AVPlayer`
  whose item feeds an `AVPlayerItemVideoOutput` of IOSurface-backed BGRA pixel buffers, each shown by gpui's
  `surface` element (contain). A poll on the main thread (8 ms while playing or settling a seek, else 100 ms)
  wakes the pane only for a newer frame (at least every 100 ms while playing), so a 24 fps clip redraws 24
  times a second; the pane's render takes the frame for the host time. It plays once the viewer has opened
  (the poster stands in until the first frame) and pauses when the viewer isn't drawn for 250 ms. Closing
  fades a still of the last frame. The bytes go to a cache file (`<cache>/media/<session>/<id>.<ext>`).
  - Chrome (torrz's look, scaled to a pane): the name, caption and Close on top; a frosted dark bar at the
    bottom (radius 22, inset 16, at most 760 wide) over a scrim: the scrubber (hover bubble with the time,
    press or drag seeks exactly, paused while dragged), then play/pause (replay at the end), −5 s / +5 s,
    `0:03 / 0:05`; on the right mute and a volume slider, loop, Copy (the file), Save…, Fill. Narrow bars
    drop the slider, then the skips. Fitted, the video sits clear of the title and the bar; Fill (`f`,
    double-click) gives it the whole pane under the chrome. At the end it stops on the last frame with a
    replay disc, or loops. Volume, mute, loop and fill carry to the next video while the viewer is open.
  - Idle 2.5 s while playing (not over the bar, not dragging): the chrome fades out and the pointer hides
    until it moves; a move, click or key brings it back; paused keeps it up.
  - Keys: space/`k` play/pause, ←/→ 5 s, ↑/↓ volume, `m` mute, `l` loop, `f` fill, `[`/`]` the previous/next
    image or video (the side chevrons too), Esc leaves Fill, else closes; ⌘C, ⌘S.
- **Audio row**: a compact player, full width up to 420 pt: play/pause, the name, a progress bar you can click
  or drag to seek, elapsed / duration, Save… on hover. Played through `AVPlayer` (no view); one plays at a time;
  it stops when the pane closes.
- Setup card wording: `Connect a video provider` / `Connect an audio provider`, the kind's providers.

## 6.5 Background generations

A generation can run as a background job so the conversation goes on while it renders.

- Every `generate_*` tool takes `background?: bool`; default `true` for `generate_video`, music and `generate_media`'s video and music endpoints,
  `false` for images, speech and sound effects. Missing-key setup (§3.1) and input validation happen first, in the
  call, so a background job always has a provider.
- The call returns at once: `Started media job-… (video with Higgsfield Kling 3.0 Pro); the result arrives as a
  message when it's done. Keep talking with the user; don't wait or poll for it.` Metadata
  `{job_id, background: true, kind, provider, model, n?, aspect_ratio?}`.
- It is a job in the session's existing registry (the one `shell` background jobs use, generalized: a job has a
  label instead of only a shell command): listed by `job.list` (`JobInfo.command` = `generate_video: <prompt>`,
  plus `JobInfo.kind`: `shell` | `media`), stopped by `job.kill` (which cancels the provider request), and on
  completion it delivers a report through the session mailbox (starting a turn when the session is idle), like a
  background shell:
  `Background media job-… (video) finished.\n<the normal result text with its [generated …] lines>` or
  `… failed: <reason>` / `… was stopped`.
- Progress: the job keeps the latest progress text (`Generating with Higgsfield… 40 %`), reported in
  `JobInfo.progress` and by the `job` tool's status.
- The user can also detach a running foreground generation like a shell command (`shell.detach` generalized,
  its name kept), turning it into a background job: `Started media job-… (…): the user detached this generation…`.
- As built: the registry is `otto_tools::jobs::Jobs` (shared by `shell`), reached by the media tools through
  `ToolCx.jobs`; without one (no session) a generation runs in the call. A restart reports a media job that never
  ended as `Background media job-… (video) was stopped (the otto server restarted).`
- Agent block: the call's placeholder tile stays (with the job's progress and elapsed time) after the call
  returns, until the job's report arrives; then the media appears in that same place (the report's `[generated …]`
  lines are matched to the call by job id). The report message itself is shown as a quiet notice, not a second
  copy of the media. The jobs bar lists media jobs with their kind icon and a stop button.

## 7. Later

- Edits from the viewer ("Edit this image…" prefills the composer with the attachment).
- Cost: per-image and per-second prices in the result metadata and the usage summary.
