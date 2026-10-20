# Remote browser: server-owned engine, client-side compositing

Status: **design + spike plan, nothing decided beyond Spike A.** Builds on PLAN.md §6 and PHASE6.md. Written
2026-10-21 against the code as it stands (`silo-web` on `cef =154.5.0` / CEF `154.0.34`).

Goal, in the user's words: best UX, not cheapest implementation. Browser **execution and state** belong to the
server; **compositing and scrolling** belong to the client. First target: two Macs (Mac mini as server, the
current Mac as client). Linux server later. No Linux desktop client is requested.

This document separates three things on purpose:
- **Target architecture:** what we want the system to look like.
- **Proven mechanisms:** what exists today in Silo, stock CEF or Chromium and is known to work.
- **Hypotheses:** what Spike A has to prove or kill. Most of the interesting parts of this design are here.

---

## 1. Where we are

PHASE6 shipped the opposite split:
- **The client runs the engine.** `silo-web` (`crates/silo-web`, ~2.6k lines) is windowless CEF with
  `shared_texture_enabled`; `on_accelerated_paint` hands over an IOSurface that `frame.rs` wraps as a
  `CVPixelBuffer` for gpui's `window.paint_surface`. The engine lives in `silo-app` (`web.rs`), pumped from a
  gpui timer, and dies when the app quits.
- **The server holds metadata only:** `BrowserState` (URL, history, title, favicon, zoom, profile) in
  `silo-server/src/browser.rs`, reported by the owning client (`browser.report`) and driven by
  `browser.navigate/back/forward/reload/zoom`. "The server never runs an engine."
- **Remote localhost** reaches the server through `silo/tunnel/1` and a client-side proxy.

Consequences the user wants to remove:
- Quitting the app or switching machines reloads every page: JS state, websockets, scroll, form contents and
  media are gone. Terminals survive app quit; browsers do not.
- Cookies and logins live on whichever client opened the page, not with the workspace.
- Two clients on one block run two engines that only share a URL.
- Remote dev servers need the tunnel and its proxy-auth machinery.

## 2. Target architecture (unified for local and remote)

```
 ┌──────────── server host ───────────────────────────────────────┐        ┌──────── client (Silo.app) ─────────────┐
 │ silo (core daemon, no CEF)                                      │        │ GPUI app                                │
 │   registry · BrowserState · ownership · tickets · persistence   │        │   BrowserPane: chrome, input, IME UI    │
 │        │ spawns / adopts (unix socket)                          │        │        │                                │
 │ silo-browser (worker, optional package, CEF/Chromium fork)      │        │ silo-compositor (sandboxed helper)      │
 │   Blink: DOM · JS · style/layout · paint · profile/cookies      │ commit │   compatible cc impl / Viz-like         │
 │   network (server-private hosts are just localhost here)        │ stream │   compositor · raster · GPU             │
 │   [cut point: TBD by Spike A] ───── resources + commits ────────┼───────►│   local scroll / pinch / compositor     │
 │                         ◄──── input, scroll deltas, viewport ───┼────────┤   animations → IOSurface → GPUI         │
 └─────────────────────────────────────────────────────────────────┘        └─────────────────────────────────────────┘
   local:  same machine; unix socket for control + shared memory / IOSurface (mach ports) for resources
   remote: iroh QUIC data stream (`data.connect`, ticketed) carrying serialized resources and commits
```

**One ownership model, two transports.**
- The browser worker runs on the machine that owns the workspace, **also for local workspaces**. The app is
  always a client; it never owns page lifetime.
- Local and remote clients speak the same logical protocol: viewport/ownership, input, commits, resources,
  acks. What differs is how a resource is carried:
  - **Local:** resource handles are shared memory regions or IOSurfaces passed by mach port (Chromium itself
    shares IOSurfaces across processes this way; `SCM_RIGHTS` on a unix socket cannot carry mach ports, so the
    local resource channel is mach/XPC-based, the control channel stays on the unix socket). No forced
    serialization, no copies of pixel or tile data.
  - **Remote:** resources are serialized, compressed, bounds-checked byte streams over the iroh data stream.
- The data plane reuses the existing pattern: `data.connect {block_id, subprotocol: "browser/1"}` → single-use
  ticket → `Hello` on a new stream. Browser blocks today reject `data.connect`; that changes.

**Can the same split be used for local workspaces? Yes, as a target, with honest costs:**
- Gains: pages survive app quit/relaunch/crash, one engine per block shared by all clients, cookies live with
  the workspace, one code path for local and remote UX (local is the zero-latency case of the same protocol).
- Costs: an extra process hop for input and frames (small locally, but nonzero and must be measured against the
  in-app engine), two resource transports to maintain, and the worker must run while no app is open (power,
  memory, launchd session; §8).
- Until Spike A passes, **local workspaces keep the current in-app engine.** If the vector/tile cut fails but the
  ownership win is still wanted, local could move to "worker owns the engine, client composites the worker's
  IOSurfaces" (option P below), which stock CEF OSR nearly supports already. That is a fallback, not the target
  for remote.

## 3. Proven vs. hypothesis

| Claim | Status |
|---|---|
| Windowless CEF on macOS delivering IOSurfaces into GPUI | **Proven** in Silo (`silo-web`, offscreen example) |
| Server-held browser metadata, ownership, persistence, hot-upgrade passthrough | **Proven** (`silo-server/src/browser.rs`, PHASE6 P6-1) |
| iroh data streams, tickets, seq'd events | **Proven** (terminals) |
| CEF OSR exposes a display list / layer tree / compositor frames | **False.** OSR `on_paint`/`on_accelerated_paint` deliver **pixels** (a CPU buffer or a shared texture). There is no stock CEF API for a compositor-only browser |
| A page's cc layer tree can be committed across a network to a client-side `LayerTreeHostImpl` | **Hypothesis.** `LayerTreeHost` (main thread) → commit → `LayerTreeHostImpl` (impl thread) assumes one process, a shared proxy (`ProxyMain`/`ProxyImpl`), synchronous commit handshakes and impl→main feedback (scroll deltas, animation events) on every `BeginMainFrame`. "Just move LTHI to the client" is not an existing seam |
| Chromium's paint/Skia serialization is network-ready | **Hypothesis, probably partly false.** Out-of-process raster serializes `PaintOp`s and uses a transfer cache and Skia's remote glyph cache (strike server/client) between renderer and GPU process. That path validates input from an untrusted renderer, but it was built for same-version, same-machine IPC with shared memory and low latency, not for persistence, loss, reconnect or bandwidth budgets |
| All scrolling, animation and caret become local | **False as stated.** Only compositor-eligible scrolling/pinch and compositor-driven animations can be local (§5). Caret, selection, accessibility, IME and typing prediction are separate work |
| The client can be a tiny universal renderer | **False.** The client needs a substantial compositor/raster/GPU runtime that matches the server's engine version (§6) |
| The server needs no GPU | **Not promised.** Blink still lays out and paints; video, WebGL, WebGPU, accelerated canvas and readbacks may still need GPU or software raster on the server (§6) |

Prior art exists (Chromium's own Blimp project, removed from the tree years ago; commercial remote browser
isolation using pixel streaming or DOM mirroring). We make no claims here about why Blimp was removed, or about
novelty or patents of this design.

## 4. The pipeline and candidate cut points

Chromium (simplified, per renderer widget):

```
Blink main thread: DOM → style → layout → paint (PaintOps, display items) ─┐
cc main: LayerTreeHost, property trees ── commit ──► cc impl: LayerTreeHostImpl (pending/active tree, input
                                                     handler, scroll, compositor animations, tiling)
                                                        │ raster (GPU raster in the GPU process: PaintOps → tiles)
                                                        ▼
                                                     CompositorFrame (quads + resources) ──► Viz (GPU process):
                                                       SurfaceAggregator (OOPIFs, video, browser UI),
                                                       hit-test regions, display compositor → screen / OSR
```

Candidate cuts, each a different product:

| | Cut | Server sends | Client runs | Local on client | Stock? |
|---|---|---|---|---|---|
| **P** | Final pixels (today's OSR) | Frames (video-encoded when remote) | Video decode + GPUI | Nothing page-related; scroll is a round trip | **Yes** (CEF OSR); remote encode is ours |
| **T** | Viz `CompositorFrame` + rasterized tiles | Quads, property/transform data, tile bitmaps (compressed) | Viz-like aggregator + a scroll/animation layer | Scrolling within already-rastered tiles, compositor animations, maybe pinch (blurry until re-raster) | No: Chromium patches |
| **V** | cc commit (layer tree, property trees, PaintOps/display lists) | Commits + PaintOp streams, images, glyphs | cc impl + raster + Viz-like compositor + GPU | Compositor-eligible scroll/pinch with sharp re-raster at any zoom/DPR, compositor animations, local tiling | No: Chromium patches, deepest |

Notes:
- **P** has the worst scrolling UX at 50–150 ms RTT (every scroll is a round trip, plus encode latency and
  compression artifacts on text) but is the smallest change and the only stock path.
- **T** may capture most of the scroll UX: the server pre-rasters beyond the viewport and the client scrolls
  locally; new content arrives as tiles. Costs: bandwidth on fast scrolls into new content, blurry pinch until
  the server re-rasters, server raster cost (GPU or CPU), and Retina tiles are 4x the bytes.
- **V** gives the best fidelity (client rasters at its own DPR and zoom, text stays sharp) and probably the
  lowest steady bandwidth on text-heavy pages, but it is the deepest fork, needs the most client runtime and has
  the most protocol surface exposed to an untrusted server. **It is not assumed to be the only good path.**
  Spike A compares T and V on the same pages before picking.
- In all three, OOPIFs mean **several** frame sinks per page (one per cross-site frame widget plus the main
  frame). A cut above Viz must carry each and redo Viz's surface aggregation and hit testing on the client.

## 5. What can be local, and what cannot

**Can be local (if the cut is T or V and the page cooperates):**
- Scrolling of scrollers the compositor can scroll on its own: no main-thread scrolling reasons, no blocking
  (non-passive) wheel/touch listeners in the hit region, compositor-scrollable overflow. Includes nested scroll
  chaining and `overscroll-behavior`, which the impl-side input handler implements today.
- Pinch zoom (visual viewport).
- Compositor-driven animations: transform/opacity CSS animations and transitions, some scroll-driven
  animations, `position: sticky`/fixed handled by property trees.

**Server round trip regardless of cut:**
- Anything that runs JS or needs layout: click handlers, hover styles that change layout, `:hover` repaint,
  JS-driven scrolling/animations, `requestAnimationFrame` effects, virtualized lists that render on scroll.
- Scrolling where the compositor must wait for the main thread: blocking wheel/touch listeners, main-thread
  scroll reasons. The client can scroll speculatively and **reconcile** when the server answers (that
  reconciliation is a Spike A test, including `preventDefault` arriving late).
- Checkerboarding: scrolling past what the server has committed/rastered shows blank or low-res content until
  the server catches up.

**Separate work items, not solved by compositing:**
- **Caret:** blink and position are Blink/main-thread state painted into layers. A local caret is a prediction.
- **DOM selection:** selection is DOM state on the server; drag-select is a round trip unless predicted.
- **Accessibility:** the AX tree lives with the DOM; it must be serialized to the client (Chromium has AX tree
  update serialization for browser↔renderer IPC, unproven over a network) and mapped to NSAccessibility.
- **IME:** composition happens on the client (NSTextInputClient in GPUI) and is forwarded; candidate window
  bounds come back from the server. Works today locally; adds RTT remotely.
- **Typing prediction:** local echo in text fields (Mosh-style, like Phase 5) is a later, separate design.
- **Hit testing:** input targeting (which frame, which scroller, cursor shape) needs client-side hit-test data
  (Viz hit-test regions for OOPIFs, cc scroll/touch-action/wheel-handler regions). Cursor shape for hover over
  links is a round trip today and stays one unless hit regions carry it.

## 6. Media, GPU and the client runtime

- **Video, WebGL, WebGPU, accelerated 2D canvas** produce GPU textures/video frames on the server, outside the
  display list. Options per layer: (a) encode that layer as a video sub-stream (server needs a hardware encoder or
  CPU budget), (b) for `<video>`, send compressed media to the client for decode (hard with MSE, EME/DRM,
  and sync), (c) fall back to P for the whole page. Spike A only checks that these degrade visibly and
  correctly; no promise of local playback.
- **Server GPU cost** does not go away: layout and paint are CPU, but accelerated canvas, WebGL/WebGPU, video
  decode, `getImageData`/screenshot readbacks and T-style raster all want a GPU or software raster. A Mac mini
  has a GPU; a headless Linux server may only have SwiftShader/software paths (later problem).
- **Client runtime** for T/V is a real chunk of Chromium: cc impl, Viz pieces, Skia, the GPU command/raster
  stack, image decoders, glyph handling. It ships as its own framework build from the same fork, inside a
  sandboxed helper, not a small universal renderer.
- **Fonts/text:** with V, glyphs are shaped and laid out on the server (layout decides line breaks), so the
  client must reproduce the server's glyphs, not its own fonts. The Skia remote glyph cache model (send glyph
  images/paths, not font files) is the candidate; whether it survives the cut is a spike item. On a Linux server
  pages will lay out with Linux fonts and look different from a Mac-hosted page; that is accepted or mitigated
  with server-installed open fonts and web fonts. Shipping Apple's system fonts to Linux is **not** an option.

## 7. Engine engineering

There is no stock API for any of T/V. Doing it means:
- **A CEF source fork plus a Chromium patch set** (cut point, commit/resource export, input/scroll feedback
  injection, client-side compositor entry point), built from source for each Chromium milestone we move to.
- **A custom framework distribution** for both the worker (server) and the compositor (client), signed and
  notarized like today's CEF framework. `cef-rs` prebuilt downloads stop being enough; the new entry points need
  our own C API and Rust bindings on top of `cef-rs` or alongside it.
- **Exact version pinning:** server worker and client compositor must run the identical engine revision. The
  serialized formats are internal Chromium structures with no compatibility promise. Add the engine revision to
  the handshake (like `GHOSTTY_REV` in `BUILD`, PLAN §3.5) and reject mismatches.
- **Untrusted stream:** the client must treat everything from a server as hostile (a remote host can be
  compromised). Every length, index, resource id, transform and image size is bounds-checked before use, with
  memory and resource-count budgets per block; deserialization and raster run in a sandboxed helper process,
  never in the GPUI app process; the deserializers are fuzzed.
- **Maintenance:** each Chromium milestone can move cc/Viz internals. Spike A measures patch footprint
  (files, lines, subsystems) and rebase effort; that number is a go/no-go input, not an afterthought.
- **Build capacity:** a Chromium/CEF source build wants a fast machine and plenty of RAM. Planning budget
  (approximate, our own estimate, not measured): 110–250 GB of disk in total for checkout, build outputs and
  distributions; recommend 250–300 GB free on the build machine. Builds and CI for the fork are a standing cost.

## 8. Lifecycle (local and remote)

**Processes:**
- `silo` (core daemon) never links CEF/Chromium. `silo-browser` is a separate binary from an optional package,
  spawned on first browser block, supervised by the server over a unix socket, and adopted (not inherited) by a
  successor server.
- The worker is headless: no windows, no Dock icon. It survives the client app quitting, like PTYs do.
  On macOS, GPU access from a launchd agent outside a GUI login session is an open question (§12).

**What survives what:**

| Event | Kept | Lost |
|---|---|---|
| App quit/relaunch, client crash, network drop | Everything: worker keeps running, client reattaches and gets a fresh full commit/resources | Client-side caches only |
| Renderer (page) helper crash | Block, `BrowserState`, profile | That page's JS/DOM state; page shows crashed, reload on request |
| Worker crash or worker upgrade | Session tree, `BrowserState` (URL, history, zoom), profile (cookies, storage) | All running page state; pages reload. Chromium's per-entry page state may restore some scroll/form data; **arbitrary JS state is never restored** |
| Server hot upgrade (`--takeover`) | Worker keeps running; new server reconnects to it | Nothing, if the worker handshake still matches |
| Machine reboot | As worker crash | As worker crash |

- **Persisted:** sessions/windows/blocks, `BrowserState`, profile directories. **Running state** (DOM, JS heap,
  websockets, media, live scroll offsets) exists only in the worker process.
- **Server takeover is not worker takeover.** The existing `SCM_RIGHTS` handoff passes PTY masters, the
  listener and checkpoints; it does not and should not carry a browser. The worker is its own process the new
  server re-adopts via its socket/runtime file.
- **Worker upgrades are separate from server upgrades** and are user-visible (pages reload). Proposal: the
  browser data subprotocol carries an engine build id separate from the core `BUILD`, so a core-only upgrade
  does not force a worker restart; a worker upgrade waits for idle or asks the user.

## 9. Multiple clients and viewport ownership

- A page has one layout viewport size, one device scale factor, one focus, one zoom: same problem as one PTY
  size. Reuse block ownership (PLAN §3.6): the owner's size/DPR/zoom/focus apply; the app takes ownership on
  focus or input; passive viewers request.
- Scroll offset is page state, so it is shared: the owner's local scrolls are reconciled into the server and
  other clients follow. Per-client scroll positions are rejected for v1 (they would diverge from what JS sees).
- Non-owners render at the owner's viewport, scaled/letterboxed with a "sized by <client>" badge, like terminals.
  Input from a non-owner takes ownership first.
- With V, a non-owner at a different DPR can still raster sharply from the same display lists; with P/T it gets
  the owner's pixels scaled.

## 10. Security and compatibility

- **Threat model change:** cookies, logins and storage move from the client to the server host. A paired device
  that can open a browser block on a host can use that host's logged-in sessions. This must be explicit in the
  UI (profile shows which host it lives on) and gated by the existing owner/paired-device grants.
- **Egress moves too:** remote pages fetch from the server's network (its IP, VPN, geo). Server-private dev
  servers just work without `silo/tunnel/1`; public sites see the server, not the client. Users must be able to
  tell.
- **Local:** app and worker come from the same bundle/runtime dir (`<SILO_HOME>/runtime/<build>/`), so versions
  match; the local socket keeps the uid check; IOSurface/mach-port sharing is only to the verified local peer.
- **Remote:** ticketed data streams, engine-revision handshake, bounds-checked stream, sandboxed client helper
  (§7). No credentials are ever copied between machines; logging in happens inside the worker via the client UI.

## 11. Deployment

- macOS: `silo-browser` ships inside Silo.app as an optional component (with its framework and helpers), copied
  to the runtime dir like `silo`, started by the server on demand. The client compositor helper ships in the
  same bundle.
- Servers without a GUI (Linux later): a separate `silo-browser-<os>-<arch>` package, installed on request
  (`silo remote install` gains an option); the core `silo` tarball stays small and CEF-free.
- First proof is Mac-only: Mac mini server, current Mac client.

## 12. Spike A: feasibility by record and replay

Purpose: decide go/no-go on T and V **before** building anything production-shaped. No production code, no
user-facing feature, throwaway branches.

**A0. Source audit (paper, no build).** On the Chromium revision matching our CEF line:
- Map the cc main/impl boundary (`LayerTreeHost`, proxies, commit, `BeginMainFrame` feedback), the
  `CompositorFrame`/`LayerTreeFrameSink` boundary, PaintOp serialization and the transfer cache, the Skia remote
  glyph cache, Viz surface aggregation and hit testing for OOPIFs, and how CEF OSR hooks into the browser
  compositor.
- Output: candidate cut points for T and V, what each must serialize, estimated patch footprint, and anything
  that kills an option outright.

**A1. Baseline.** Measure the current in-app engine and a P-style stream (worker OSR frames encoded and sent to
the client) on the same pages and conditions. Every later number is compared to these.

**A2. Capture (build machine).** Patched worker loads one simple page (a static docs page, then a GitHub repository
page), records commits/frames and all referenced resources to a file.

**A3. Replay (current Mac).** A standalone replay harness, in a sandboxed process, renders the capture. Check:
text and page fidelity against a reference screenshot of the same page, Retina (DPR 2), browser zoom levels,
and pinch.

**A4. Live scroll.** Connect worker and client live: client scrolls compositor-eligible content locally, sends
deltas, the server applies them and later commits reconcile. Observe snap-back, checkerboarding, jank.

**A5. Exploratory pages.** Nested scrollers with chaining; a blocking (non-passive) wheel listener that sometimes
calls `preventDefault`; a cross-origin iframe (OOPIF) that scrolls; sticky headers; CSS animations; a `<video>`
and a WebGL canvas (expected to degrade; check that it degrades correctly).

**A6. Network.** At 0, 50 and 150 ms RTT (emulated on the client link), plus some loss:
- time to first frame (attach to a running page, and cold load);
- input latency: scroll start, click → visible change, key → glyph in a text field;
- bandwidth: idle, reading, fast scroll, page load;
- reconnect: drop the connection mid-scroll, resume; restart the worker.

**A7. Cost.** Patch footprint (files, lines, subsystems touched), effort to rebase one milestone, client runtime
size, server CPU/GPU/RAM per page.

**Sites:** GitHub and docs pages first. Gmail later, only after the user logs in explicitly inside the worker;
no cookies or credentials are copied from any existing browser profile.

**Go/no-go criteria:**
- **Go** requires all of:
  - replay is visually indistinguishable from local rendering for text pages at DPR 2 and common zoom levels;
  - compositor-eligible scroll starts locally with no RTT-dependent delay at 50 and 150 ms, and reconciliation
    never produces visible snap-back on the test pages;
  - blocking listeners, nested scrollers and OOPIF behave correctly (local where allowed, correct where not);
  - reconnect restores the page without reload;
  - patch footprint and rebase effort judged maintainable by the user.
- **No-go** for an option if any of: the cut needs pervasive changes across Blink/cc/Viz; fidelity needs the
  client to have the server's fonts; the client cannot be sandboxed; the stream cannot be bounds-checked without
  rewriting the deserializers.
- **Proposed targets (not commitments, to be revised by A1 baselines):** first frame on attach within roughly the
  same order as the in-app engine's cold load at 0 ms RTT; steady reading bandwidth well below a P-style video
  stream of the same page; no unbounded memory growth on the client under a hostile stream.

**Outcomes:** V go → plan V. Only T go → plan T. Neither → keep the in-app engine for remote; optionally move
local workspaces to option P for page survival (§2).

## 13. Comparison for UX

| | P: pixels/video | T: tiles + local compositor | V: display lists + local compositor | Today: client engine |
|---|---|---|---|---|
| Scroll at 150 ms RTT | Round trip | Local within rastered area | Local, re-raster on client | Local |
| Text sharpness | Codec dependent | Server DPR only | Client DPR, any zoom | Native |
| Pinch zoom | Round trip | Local, blurry until re-raster | Local, sharp | Native |
| Bandwidth | Constant while changing | High on new content | Lowest expected for text, high for images | Network to sites |
| Page survives app quit | Yes | Yes | Yes | No |
| Shared across clients | Yes | Yes | Yes | No |
| Client runtime | Decoder | Compositor | Compositor + raster + GPU | Full engine |
| Engine work | Small (stock OSR) | Chromium patches | Deepest patches | Done |
| Version coupling | Loose | Exact | Exact | None |

## 14. Open questions

- Which CEF/Chromium line to fork, and whether to keep `cef-rs` underneath custom bindings.
- Can a launchd-agent worker on macOS get a GPU without a GUI session on the server? (The mini will usually be
  logged in, but that must not be required.)
- Mach-port resource sharing for local transport: XPC service vs. bootstrap-registered port.
- Whether accessibility can reuse Chromium's AX tree serialization across the cut.
- Profile model once cookies live on servers: one profile per host, or named profiles per host.

## 15. Machines for Spike A

Decided 2026-10-21:
- **Build machine: AWS EC2 `mac-m4pro.metal` (`silo-build`).** Apple M4 Pro, 14 cores, 48 GB RAM, 1 TB
  encrypted gp3 volume, macOS 26 Tahoe, us-east-2. Holds the Chromium/CEF checkout and builds, and runs the
  patched worker for A2. Billed per hour while the dedicated host is allocated (24 h minimum).
- **Live server: the user's Mac mini (`batmini.local`).** Used from A4 on with the built worker package copied
  over; it never holds the Chromium checkout. Its specs are not yet checked (SSH auth was not set up).
- **Client: the user's MacBook.** Replay harness and compositor.

Agent rules for this branch are in `AGENTS.md`.
