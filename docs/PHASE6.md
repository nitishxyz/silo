# Phase 6: browser blocks

Goal: a block can be a browser.
- **Server holds the state:** URL, history, title, favicon, zoom and profile.
- **Client runs the engine:** CEF windowless, composited into GPUI.
- **Localhost URLs** reach the server machine over `silo/tunnel/1`.

Planning doc. It builds on PLAN.md §6/§7 and the code as of the Phase 5 plan.

## 1. What exists (checked 2026-10-19)

| Thing | Finding |
|---|---|
| crates.io `cef` | **154.5.0+154.0.34** (2026-10-07), repo `tauri-apps/cef-rs`. Supports macOS x86_64/arm64. Its `build.rs` (`cef-dll-sys`) downloads the CEF binary distribution into `OUT_DIR`, or uses `CEF_PATH`. It ships `bundle-cef-app` for macOS `.app` + helper layout. The `accelerated_osr` feature pulls in `objc2-io-surface`/`objc2-metal` (and wgpu); we don't need it, because we only read `AcceleratedPaintInfo.shared_texture_io_surface` |
| `cef-rs` | Same project as the crate above; there is no separate binding to choose |
| `chromiumoxide` 0.9.1 | CDP client for an external Chrome. It can't embed or composite in-process. Fit only for the later server-side headless/agent flavor |
| gpui fork `b36b2e8` | macOS `Window::paint_surface(bounds, CVPixelBuffer)` (`gpui/src/window.rs:4856`), `gpui::surface()` element, `surface_bgra_fragment` in `gpui_macos/src/shaders.metal` via `CVMetalTextureCache`. **Limits:** BGRA is drawn opaque (alpha forced to 1), and the clip is a rectangular `content_mask` only, with **no corner radii** |
| desktop-platform | Found at `~/dev/nitishxyz/desktop-platform` (not `~/dev/desktop-platform`). `crates/web-runtime` (~14k lines incl. examples) and `crates/webview` (~2.6k), pinned to `cef =151.8.0`. Linux-only design: `multi_threaded_message_loop: true` (**unsupported on macOS**), DMA-BUF `on_accelerated_paint`, and no IME bridge |

**Portable from desktop-platform:**
- `web-runtime/src/session/{handlers,types,shared,geometry,input,popup}.rs`: client handlers, frame queue, popup
  handling, input translation.
- `lifecycle.rs`, `dialog.rs`, `file_dialog.rs`, `context_menu.rs`.
- `webview/src/{pump,keyboard,input,frame}.rs`.

**Not ported:**
- `semantic_document.rs`, `semantic_budget.rs` and the `platform-agentkit-*` dependencies (agent features, which
  are Phase 7+).
- `platform-native-*` UI.

## 2. Engine path: decided

**Windowless CEF + accelerated paint → IOSurface → CVPixelBuffer → `window.paint_surface`.** One engine, one
compositing model, and the same input path as the terminal.

| | Windowless CEF in GPUI | WKWebView child NSView |
|---|---|---|
| Compositing | Drawn in the GPUI scene, so z-order is correct by construction | A separate layer above the Metal view; anything GPUI draws over it (palette, switcher, tooltips, drag ghosts, menus) is hidden *under* it unless we hide/snapshot the view |
| Rounded pane corners | Needs a fork change (below) | `layer.cornerRadius` + `masksToBounds`, so easy |
| Motion (`motion.rs` ghosts, zoom animations) | Works: frames are just textures | Has to be moved/resized in sync each frame, and lags or tears |
| Input | We translate GPUI events to CEF (port `input.rs`/`keyboard.rs`), with full control over our shortcuts | Native, but steals first responder; our keybindings need an `NSEvent` monitor |
| IME | We bridge `EntityInputHandler` → `CefBrowserHost::ime_set_composition/commit_text` (new work) | Native, free |
| Size/packaging | +~250–300 MB framework, 4 helper apps, signing work (§7) | 0 MB |
| Profiles, devtools, CDP | Per-profile `RequestContext`, devtools, remote debugging port (agent path) | `WKWebsiteDataStore(forIdentifier:)` (macOS 14+), Safari Web Inspector, no CDP |
| Engine parity with Linux client later | Same | Different engine |

WKWebView would win on IME, size and corners. It loses on the thing Silo is built around: panes that animate,
overlap overlays and stay in one scene. So **CEF**, with two pieces of fork work:

1. **Rounded/alpha surfaces:** add `corner_radii` to `PaintSurface` and apply the existing quad SDF in
   `surface_vertex`/`surface_bgra_fragment` (plus `paint_surface_rounded(bounds, radii, buf)`). It's a small
   change to `nitishxyz/zed` (`torrz/glass`). Until it lands, the pane card paints its 1 px border and corner
   "caps" over the surface corners, which looks acceptable at the current radii.
2. **`CrAppControlProtocol` on NSApp:** CEF on macOS requires the `NSApplication` subclass to implement
   `isHandlingSendEvent`/`setHandlingSendEvent:` and wrap `sendEvent:`.
   - gpui's `GPUIApplication` (`gpui_macos/src/platform.rs:75`) doesn't.
   - Fix: add the methods in the fork's `ClassDecl`. An objc runtime `class_addMethod` from our crate is possible,
     but the fork is cleaner. **This blocks CEF init.**

**Message loop:**
- `multi_threaded_message_loop` is unsupported on macOS, so we use `external_message_pump: true`.
- `CefApp::on_schedule_message_pump_work(delay)` records the due time and wakes the app's pump loop
  (`silo-app/src/web.rs`, a `Notify`, no polling), which calls `cef::do_message_loop_work()` when it's due,
  plus a 30 Hz safety tick while any browser exists: CEF never re-announces delayed work it saw during a
  pump (the old 4 ms poll woke the main thread 250 times a second, browsers or not).
- Paint callbacks arrive on the UI thread, so there are no mutexes on the hot path. A paint marks its pane,
  which the loop notifies right after that pump.

**Frames (external begin frames):**
- CEF's own frame timer runs as delayed tasks on the UI thread and missed most frames while a page
  scrolled: 18–35 fps with 80–135 ms gaps, at any `windowless_frame_rate`, however the pump ran. So
  every page has `external_begin_frame_enabled` and paints only when asked (`Browser::begin_frame`).
- A shown pane asks on each refresh of its window (`window.on_next_frame`, `web::Frames`) while it is
  active: it painted or got input in the last second (input with no loop running asks once at once).
  An idle page, a hidden tab's (5 fps) and the off-screen host's get frames from `Runtime::drive_pages`
  on the pump's tick, so a change still shows and wakes the refresh loop.
- `windowless_frame_rate` is the display's refresh (`display_rate`: the fastest screen's
  `maximumFramesPerSecond`, 60–240): CEF captures no more frames than that, asked or not.
- `cargo run -p silo-web --example offscreen -- --perf` measures it (800x600@2x text page, a 120 Hz
  display, 2026-10-28): scrolling at 120 Hz input painted 18–22 fps (paint interval p50 35 ms, p95
  135 ms) before, 116–118 fps (p50 8.3 ms, p95 10 ms) after; wheel → paint p50 34 → 20 ms; rAF 60 →
  120 fps.

**Accelerated paint on macOS:**
- `on_accelerated_paint(info)` hands over `info.shared_texture_io_surface`, which is valid only during the
  callback.
- We `CVPixelBufferCreateWithIOSurface` (retaining the IOSurface), store it as the pane's current frame and
  `cx.notify()`. The surface element paints the latest buffer (gpui's `CVMetalTextureCache`: no copy).
- CEF paints into 3 surfaces in turn: the one held is painted again 2–3 frames later at the soonest, after
  gpui drew it (`--perf` prints it).
- Software `on_paint` isn't implemented (no fallback). Init fails loudly if `shared_texture_enabled` isn't honoured.

## 3. Server-held state (proto, registry, persistence)

- `BlockKind` (already `Terminal`-only in `silo-proto/src/model.rs:269`) gains `Browser`.
- `BlockInfo` gains `browser: Option<BrowserState>`, which is `Some` exactly when `kind == Browser`.
- Terminal-only fields stay as they are; for browsers, `size`/`pid` are unused (zero/`None`).

```rust
pub struct BrowserState {
    pub url: String,             // committed URL of the current entry
    pub history: Vec<HistoryEntry>, pub index: usize,  // back/forward list, capped at 50
    pub title: String,
    pub favicon: Option<String>, // data: URL, ≤ 32 KiB, PNG 32x32
    pub zoom: f32,               // CEF zoom level, 0.0 = 100%
    pub profile: String,         // "default" | named; maps to a client-side RequestContext dir
    pub loading: bool,
}
pub struct HistoryEntry { pub url: String, pub title: String }
```

**Who writes it:**
- The **owning client** (same ownership as terminals, §3.6 of PLAN) reports engine state with
  `browser.report {block_id, url, title, favicon?, loading, history, index}`. The server stores it, bumps the
  revision and emits `block.changed`.
- **Commands** (`browser.navigate/back/forward/reload/zoom`) go to the server. It updates the intended state and
  emits `browser.command {block_id, command}` to the owner (and to every attached client, so passive viewers
  follow).
- **The server never runs an engine.**
- With no client attached, the commands update the stored state directly (`navigate` pushes history), so the next
  client opens there.

**Methods** (`silo-proto/src/control.rs`):
- `block.split {block_id, axis, spawn, kind?: BlockKind, url?: String}` and `window.create {.., kind?, url?}`.
  `kind` defaults to terminal.
- `browser.navigate {block_id, url}` (bare host → `https://`; `localhost`/private-IP → `http://`),
  `browser.back`, `browser.forward`, `browser.reload {ignore_cache?}`, `browser.zoom {block_id, zoom}`,
  `browser.report {…}`.
- `data.connect` on a browser block is rejected with `bad_request`. Browser blocks have no data stream.

**Registry/actor:**
- A browser block has no PTY actor. `silo-server/src/browser.rs` (new) holds a `BrowserBlock { info }` behind
  the same `BlockHandle`-shaped interface, as an enum: `BlockHandle::{Terminal, Browser}`.
- Registry calls that are terminal-only (`terminal.*`, ownership size logic) return `bad_request` for browsers.

**Persistence:** `persist.rs::BlockRecord` already stores `BlockInfo`, so `browser` round-trips with no
checkpoint file (`generation: None`). Hot upgrade passes it through `handoff.rs` unchanged.

## 4. Client: `silo-web` crate + browser pane

**`crates/silo-web` (new)** is the CEF boundary and the only crate with `unsafe` for CEF/CoreVideo:
- `Runtime::init()`:
  - CEF settings: `windowless_rendering_enabled`, `external_message_pump`, `root_cache_path =
    ~/Library/Application Support/Silo/web`, one `RequestContext` per profile under `web/profiles/<name>`.
  - Must run before gpui's `Application::run` returns control, and after `CrAppControlProtocol` is in place.
  - The helper subprocess entry is `silo-app --type=…`, handled by `cef::execute_process` at the very top of
    `main.rs`.
- `Browser` handle: `navigate/back/forward/reload/zoom/resize(scale,size)/focus`, mouse/key/wheel/IME events,
  and `frame() -> Option<CVPixelBuffer>`.
- Events: `UrlChanged`, `TitleChanged`, `FaviconChanged`, `Loading`, `HistoryChanged`, `NewTab`, `ContextMenu`,
  `CursorChanged`, `Closed {by_page}`.
- **New tabs** (`silo-web/src/popup.rs`, `silo-app/src/browser_pane/tabs.rs`): one path for ⌘-/middle-/⇧⌘-clicked
  links, `target=_blank`, `window.open` and the context menu's Open in New Tab: `window.create {kind: browser,
  background}` in the page's session (a tab; `background` leaves the active tab alone).
  - Dispositions: `NEW_BACKGROUND_TAB` (⌘-click, middle click) opens behind; `NEW_FOREGROUND_TAB` (⇧⌘),
    `NEW_POPUP`, `NEW_WINDOW` (⇧-click, `window.open`) in front; downloads, PiP and the current tab are not tabs.
  - Modified link clicks reach `on_open_urlfrom_tab` (no opener, as in Chrome): cancelled there, a fresh page opens.
  - `window.open` and `target=_blank` reach `on_before_popup`: it sets windowless `WindowInfo`, this crate's
    client (a new `Shared`) and returns 0, so CEF creates the popup in the opener's process and context with
    `window.opener` intact. `Event::NewTab {popup: Some(Popup)}` hands it to the app; `Popup::open(notify)` adopts
    it. The new block starts blank (no second load); the adopting pane keeps its engine state over the
    server's until it owns the block, then reports. A window showing the block takes that pane; else the
    off-screen host runs it (also for remote hosts), so a tab opened behind loads and gets its title.
    `on_before_popup_aborted` and a popup closed before CEF made it are handled. `window.open` features
    (`size`) are reported but a tab ignores them.
  - `window.close()` in a popup: `Closed {by_page: true}` → `block.close`; the opener sees `closed`.
  - No popup blocker: popups without a user gesture open too (at most 8 pending per page).
- **Context menu:** `run_context_menu` holds CEF's callback and sends `Event::ContextMenu` (link + its text, media
  kind/src/state flags for image/video/audio/canvas, selection, editable + edit flags, misspelled word +
  suggestions, page URL). The link text comes from a per-frame script (`menu.rs` `MENU_SCRIPT`, tagged like the
  theme reporter) that also keeps the `contextmenu` target for media actions. The pane draws `context_menu.rs`
  (↑↓ ↩ esc; toggle rows show a ✓ column) with rows from `browser_pane/menu.rs`, grouped like Chrome's:
  spelling, link (New Tab, Split, Save Link As…, Copy Link Address/Text), image (New Tab, Save As…, Copy Image,
  Copy Image Address), video/audio (Play/Pause, Mute, Loop ✓, Show Controls ✓, New Tab, Save As…, Copy
  Address), editable (Undo … Select All by edit flags), selection (Copy, Search, Look Up), the page alone (Back,
  Forward, Reload, Save Page As…, Print…, View Page Source), Inspect last. Engine rows answer with
  `Browser::menu_command`; app rows cancel it.
  - Media actions (`Browser::media`) run JS on the kept target in the menu's frame (no CDP round-trip).
    No Picture in Picture: CEF reports `CAN_PICTURE_IN_PICTURE` but windowless pages have no PiP window.
  - Downloads (`download.rs`): `can_download` allows; `on_before_download` → `Event::DownloadAsked`, the pane
    answers `Browser::download_to` (Downloads, `name (2).ext` like Chrome; Save … As: gpui's `NSSavePanel`).
    Progress `Event::Download` → a pill at the pane's bottom (Cancel; then Open, Show). Never auto-opened; no
    Open for installers/scripts (`dangerous`). Chromium sets the quarantine xattr. `Browser::save_as` =
    `StartDownload` + a marker so the request is Save As.
  - Print: `BrowserHost::Print` runs the system print panel modally (works windowless).
  - Inspect (⌥⌘I; Inspect from the menu inspects the clicked element): `ShowDevTools` with a windowed
    WindowInfo (Chrome style; Alloy is refused for DevTools) in its own NSWindow; its ⌘C/V/X/A/Z and ⌘W are
    mapped by `inspector.rs` (the app menu doesn't reach that window).
  - View Page Source (⌥⌘U): `view-source:` of an http(s) page is a block URL (`browser_url`); the entry
    commits the page's URL, so `on_address_change` reports the entry's `view-source:` display URL.
  - Look Up: `showDefinitionForAttributedString:atPoint:` on the window's view.
  - Save Page As… (⌘S): downloads the page's URL (HTML only, re-fetched; no "complete" save).
- Verified by `cargo run -p silo-web --example offscreen -- --popups` and `-- --menus` (both in the full run).
- Ported from desktop-platform `web-runtime/session/*` and `webview/{pump,keyboard,input,frame}.rs`.
  DMA-BUF is replaced by IOSurface, and agentkit is dropped.

**`silo-app/src/browser_pane.rs` (new):**
- `BrowserPane` sits next to `Pane`.
- `workspace.rs` keeps `panes: HashMap<String, (PaneKind, Subscription)>`, with
  `enum PaneKind { Terminal(Entity<Pane>), Browser(Entity<BrowserPane>) }`, chosen by `BlockInfo.kind`.
- Chrome is concise: a single 28 px row in the pane with back/forward/reload, URL field (`text_field.rs`), and a
  favicon and title shown only when unfocused.
  - **Now (2026-10):** no row of its own. Back/forward, reload or stop, the security indicator, the address and the
    zoom pill are the pane header's middle (`BlockHeader for BrowserPane`, `pane_header.rs`); the page fills the rest.
    No title in the header (the tab shows it); the page load is the header's progress line. A browser's header
    can't be hidden: with every other block type's header off it shrinks to that middle (no icon, no pane buttons).
  - Cmd-L focuses the URL field; Cmd-[ / Cmd-] go back/forward; Cmd-R reloads; Cmd-+/- zoom.
- **IME:** `BrowserPane` implements `EntityInputHandler` and forwards to `ime_set_composition` /
  `ime_commit_text` / `ime_finish_composing_text`. Candidate-window bounds come from
  `RenderHandler::on_ime_composition_range_changed`. This is new work; desktop-platform had none.
- Commands from the palette/CLI: "New Browser Split" → `block.split{kind:browser,url}`.

**Gestures** (`gesture.rs`, pure state; `browser_pane.rs` wires it):
- **Pinch** (`on_pinch`) zooms like Chrome on macOS: the page's visual viewport, with no reflow and no change to
  `BrowserState.zoom`. `Browser::pinch` sends two touch points spreading around the pinch center (±200 pt, scaled
  by the gesture). They go down 16 pt inside that radius and move out to it at once: Chromium's touch slop
  swallows that first move, so the pinch measures from the full radius and lands on the scale asked for.
  Chromium's gesture recognizer turns the spread into a page-scale pinch, clamped to 1×–3× (measured in this
  windowless browser); navigation resets it. One gesture's spread is limited to 0.35×–6× of its start, so
  going from 3× back to 1× may take two pinches.
  - CEF takes no trackpad pinch (no gesture events), and CDP's `Input.synthesizePinchGesture` neither zooms a
    windowless page nor keeps up (~100 ms a step), so touches it is.
  - Scrolling must work after any pinch (`--pinch` in the offscreen example proves each):
    - Touches are always **cancelled**, never released: a release with the fingers still moving flings, and a
      fling over a cross-site frame left Chromium's touch scroll running, which took every wheel scroll after.
    - Touches left down take no wheel scroll: a step-less pinch is lifted after 300 ms (a lost end, resting
      fingers; the next step starts a new sequence), and a click, blur, navigation, hidden tab, detach or
      closed page lifts them. Scrolling while the touches are down pans them (the page follows the fingers).
    - A wheel scroll just before or during a pinch stayed latched through it (CEF adds wheel phases by a
      500 ms timer) and swallowed the next scroll while the wheel kept coming. Each pinch's end flips the
      NumLock flag on later wheel events: a modifier change starts a new wheel scroll at once.
  - After a pinch the pane reads `visualViewport.scale` back (its own count is an estimate).
- **Wheel:** CEF takes whole points; fractions carry to the next event (rounding lost slow trackpad scrolls
  and inflated others). No phases reach CEF's windowless API: it adds its own, momentum included.
- **Smart zoom** (two-finger double tap, `on_smart_magnify`; fork `SmartMagnifyEvent`): 2× at the pointer, or back
  to 1× (the narrowest spread, clamped by Chromium), as a short synthetic pinch of 12 frames (2 under Reduce
  Motion). The scale the pane tracks is an estimate; it only decides which way smart zoom goes.
- **Swipe back/forward:** only when "Swipe between pages" is on (`gpui_platform::swipe_navigation_enabled`).
  - A phased pixel scroll locks its axis after 8 pt; it is horizontal when |dx| > 2|dy|.
  - When it starts, the pane asks the page (one `Runtime.evaluate`) whether an element under the pointer
    can still scroll horizontally toward its start (a back swipe) or its end (forward). Until the page
    answers, and whenever an element would take it, the page keeps the gesture, as in Chrome.
  - After 32 pt it takes over from the page if the page's root scroll offset (`on_scroll_offset_changed`)
    hasn't moved and history steps that way. From then on the page gets no deltas.
  - An arrow chip slides in from that edge and fills with the accent at 140 pt more; it commits on release.
    Release short of that, reverse, or a cancelled gesture aborts.
  - Momentum after a navigation swipe is dropped. Mouse wheels and mouse swipe buttons are unchanged.
- **⌘+ / ⌘− / ⌘0** step page zoom (`browser.zoom`) by half CEF levels in [-3, 5]; ⌘0 resets. In the address bar
  they do nothing to the page.
- `scripts/ui-driver.swift`: `swipe x y dx [steps]` (phased horizontal pixel scroll), `pan x y dy [steps]`
  (phased vertical scroll plus momentum), `pinch x y scale [steps]` and `smartzoom x y` (synthetic gesture
  events; both reach AppKit). Check zoom with `browser.eval` of `visualViewport.scale`.

**Verified (2026-10-22, macOS, synthetic events from `ui-driver` against a local page):**
- **Pinch:**
  - 1.8 → `visualViewport.scale` 1.81 with `innerWidth` unchanged (no reflow), no text selection and no
    context menu.
  - Pinching further stops at 3×; pinching out returns to 1×.
- **Smart zoom:** toggles 2.01× ↔ 1×, repeatably.
- **Swipe:**
  - A 300 pt right swipe goes back; a 60 pt or 160 pt swipe aborts.
  - Over a horizontally scrolled element, right swipes scroll it (400 → 112 → 0) instead of navigating.
    Once it is at its start, the next right swipe goes back.
  - A forward swipe without forward history does nothing.
  - A precise vertical pan with momentum scrolls its full distance (450 px).
- **Zoom chords:** ⌘+ shows `120%` in the zoom pill, ⌘− steps down, ⌘0 resets, and ⌘0 in the address bar
  leaves the zoom alone.

**Not verified:** a real trackpad (all of the above used synthetic CGEvents); the system's two-finger secondary
click during smart zoom; right-to-left scrollers (the scroller check uses `|scrollLeft|`).

**Ghost/motion:** `ghost()` for browser panes returns the last `CVPixelBuffer`, so animations reuse the texture.

## 5. Tunnels (`silo/tunnel/1`)

These only apply to browser blocks on **remote** servers. Local servers load `localhost` directly.

**Which requests are tunneled:**
- Hosts `localhost`, `*.localhost`, `127.0.0.0/8` and `::1`, and RFC 1918 / link-local / ULA literals. These are
  "server-private".
- Public hosts go directly from the client, which is better for latency and for the user's own cookies/VPN.

**Client: a local proxy port, with no URL rewriting:**
- `silo-client/src/tunnel.rs` (new) runs one loopback HTTP proxy per remote server on `127.0.0.1:0`.
- CEF's profile `RequestContext` for that server's blocks gets a `proxy` preference
  `{mode: "fixed_servers", server: "127.0.0.1:<port>", bypass_list: "<-loopback>"}`: **all** of that profile's
  traffic goes to the client proxy (a PAC can't reach loopback, see §8). The proxy tunnels exactly the
  server-private hosts and connects everything else DIRECT itself.
  - Pages keep their real origin (`http://localhost:3000`), so cookies, CORS and HMR websockets behave as they
    would on the server.
  - Profiles are per server (`profile = "<server>/<name>"`), so localhost cookies of two servers never mix.
- The proxy accepts `CONNECT host:port` (https/wss) and absolute-form `GET http://host:port/…` (plain http and
  ws upgrade).
- For each client connection it opens an iroh connection with `TUNNEL_ALPN` to the server (cached, one per server)
  and a new bi stream, writing `TunnelOpen {host, port}` (a JSON frame, the same `u32 len | u8 kind` framing).
  It waits for `TunnelOk` / `TunnelErr {code}`, then splices bytes both ways.
  - For absolute-form http it rewrites only the request line to origin-form before splicing.

**Server:**
- `silo-transport/src/net.rs`: `serve_ctl` becomes `serve(backlog) -> (CtlIncoming, TunnelIncoming)`, which
  builds the one `Router` with both `.accept(CTL_ALPN, …)` and `.accept(TUNNEL_ALPN, …)`. The router is spawned
  once, so a second ALPN can't be added later. `TunnelIncoming` yields `(RemoteConn, BiStream)` per stream.
- `silo-server/src/tunnel.rs` (new):
  - Authorizes `conn.remote_id()` against the **same paired-device grants** (`auth.rs`, owner or paired device).
    An unknown peer gets the connection closed immediately with `unauthorized`.
  - Allows only server-private destinations (the same list as the client; checked server-side after DNS
    resolution, so `localhost` can't be pointed elsewhere).
  - `TcpStream::connect` with a 5 s timeout, then `copy_bidirectional`.
- No per-port grants: a paired device can already run shell commands, so it can already reach any local port.
- Limits: 256 concurrent streams per peer, idle streams closed after 10 min.
- Local unix clients never use tunnels.

## 6. Packaging (macOS)

- **Getting CEF:**
  - `cef-dll-sys`'s `build.rs` downloads the CEF minimal distribution for the pinned version (~120 MB archive from
    the Spotify CDN) on first build, or uses `CEF_PATH`.
  - We set `CEF_PATH=$HOME/Library/Caches/silo-dev/cef/<version>` (now `154.0.34`, ~340 MB), filled once by
    `scripts/fetch-cef.sh` (cef-rs's `export-cef-dir`). That keeps clean builds and CI offline after the first
    fetch. The workspace includes `silo-web`, so every workspace build needs `CEF_PATH` exported.
  - Build prerequisites: CMake and Ninja (`brew install cmake ninja`) for the `libcef_dll_wrapper` build.
  - Pin `cef = "=154.5.0"` exactly (build-ID rule: the version is part of `BUILD` through `Cargo.lock`).
- **Bundle layout** (extend `scripts/bundle-macos.sh`; don't adopt `bundle-cef-app`, because the bundle already
  has the server binary and the menubar):
  - `Silo.app/Contents/Frameworks/Chromium Embedded Framework.framework` (~250–300 MB unpacked, arm64);
  - helpers `Silo Helper.app`, `Silo Helper (GPU).app`, `Silo Helper (Renderer).app`,
    `Silo Helper (Plugin).app`;
  - each helper is the same small `silo-web-helper` binary (a new bin in `silo-web` that calls
    `cef::execute_process`) with its own Info.plist.
  - The main binary loads the framework through `cef::library_loader` at startup, and helpers at
    `../../../..`-relative paths.
- **Signing:**
  - Sign inside-out: framework libraries, then the framework, then each helper (with its own entitlements:
    Renderer/GPU need `com.apple.security.cs.allow-jit` and `allow-unsigned-executable-memory`; GPU also needs
    `disable-library-validation` only if we load unsigned plugins, which we don't), then the app.
  - Hardened runtime everywhere. Notarization is unchanged, just bigger and slower (~5 min more).
- **Size:** the app grows from ~40 MB to ~330 MB unpacked, and the dmg by ~110 MB. Accepted: there is no
  "download on first browser" path (one clean path).
- **Server:** unchanged. The `silo` server binary never links CEF, and the Linux server packages are unaffected.

## 7. Tasks (file ownership doesn't overlap)

**P6-1: browser block state + control API.** Headless; depends on nothing.
- **Files:** `silo-proto/src/{model.rs,control.rs}`, `silo-server/src/{browser.rs (new),registry.rs,conn.rs
  (dispatch),block.rs (`BlockHandle` enum),persist.rs,handoff.rs}`, `silo-cli` (`silo new --browser URL`,
  `silo browser navigate|back|…`).
- **Tests:**
  - split with `kind:browser` creates a block with `BrowserState{url}`;
  - navigate/back/forward edit history with no client attached;
  - `browser.report` from a non-owner gets `unauthorized`;
  - `data.connect` on a browser gets `bad_request`;
  - persist round trip + hot-upgrade handoff keep `BrowserState`;
  - `browser.command` is emitted to subscribers.

**P6-2: tunnels.** Headless; depends on nothing, so it runs in parallel with P6-1.
- **Files:** `silo-transport/src/{net.rs,tunnel.rs (new)}`, `silo-server/src/tunnel.rs (new)` + the
  `serve_ctl` → `serve` call site in `silo-server/src/lib.rs:181`, `silo-client/src/tunnel.rs (new)`.
- **Tests (two in-process iroh endpoints, relay disabled):**
  - paired peer → `TunnelOpen{localhost, P}` reaches a test HTTP server; an unpaired peer is closed;
  - a public destination gets `TunnelErr{forbidden}`;
  - the client proxy handles `CONNECT` and absolute-form GET and a websocket upgrade end to end (with `curl -x`);
  - the proxy's routing rule sends `localhost:3000` through the tunnel and `example.com` DIRECT, for both
    `CONNECT` and absolute-form requests (pure function test + `curl -x` passthrough to a public test server).

**P6-3: fork + `silo-web` runtime.** Depends on nothing. Needs a GUI session to fully verify.
- **Files:** `nitishxyz/zed` fork (`CrAppControlProtocol` on `GPUIApplication`, rounded surfaces), workspace
  `Cargo.toml` rev bump, `crates/silo-web/**` (new), `scripts/bundle-macos.sh`.
- **Tests:**
  - headless: `cargo test -p silo-web` covers the input/key translation tables;
  - `silo-web` example `offscreen_smoke` inits CEF, loads a `data:` page that paints a known color, and gets at
    least one `on_accelerated_paint` with a 32BGRA IOSurface whose centre pixel matches. It needs a logged-in
    macOS session (WindowServer), not a visible window, so it runs on a self-hosted macOS runner;
  - the bundle passes `codesign --verify --deep --strict` and `spctl -a`.

**P6-4: browser pane in the app.** Depends on P6-1 and P6-3, plus P6-2 for remote.
- **Files:** `silo-app/src/{browser_pane.rs (new),workspace.rs,layout.rs,motion.rs,palette.rs,commands.rs,
  main.rs}`.
- **Tests:**
  - unit: `PaneKind` selection from `BlockInfo.kind`; ghost for browser panes;
  - UI (`scripts/ui-driver.swift`): split browser → type URL → title appears in the tab; Cmd-[ goes back;
    palette/switcher overlays draw over the page; Japanese IME composes in a `<input>`;
  - remote: a dev server on the remote host loads at `http://localhost:5173` with HMR working.

Order: P6-1 ∥ P6-2 ∥ P6-3, then P6-4.

**Verifiable headless:** P6-1 and P6-2 fully, and P6-3's translation units. CEF paint needs a WindowServer
session, and P6-4 needs the GUI.

## 8. Blockers and risks

- **CEF init on macOS:** needs `CrAppControlProtocol` on NSApp. That's a fork change to `GPUIApplication`
  (P6-3, first step).
- **CEF binaries:** need a ~120 MB download (Spotify CDN, via `cef-dll-sys` or `export-cef-dir`) per version, and
  ~300 MB of disk per checkout. CI caches `CEF_PATH`.
- **Rounded corners:** the fork's surface shader has no corner radii and forces alpha to 1. A shader change in
  `gpui_macos/src/shaders.metal` + `scene.rs`; until it lands, caps are painted over the corners.
- **desktop-platform code:** pinned to `cef 151.8`. API drift to 154 (handler trait signatures) is expected in
  the port; budget it in P6-3.
- **IME:** brand-new code (`ime_set_composition` mapping). Main risk is candidate-window placement; tested with
  Japanese/Pinyin in P6-4.
- **Tunnel proxy (resolved in P6-3):** a PAC can't proxy loopback. With `mode: pac_script`, Chromium resolves
  `localhost`/`127/8`/`[::1]` DIRECT even when the PAC returns `PROXY` for every host (seen in the net log). A
  `bypass_list: "<-loopback>"` is dropped in PAC mode, and the global `--proxy-bypass-list=<-loopback>` switch
  doesn't help either. `fixed_servers` + `bypass_list: "<-loopback>"` does proxy loopback, but a bypass list can't
  say "private hosts only". So the profile sends everything to the client proxy, which goes DIRECT for public
  hosts (§5). The pref is applied in `on_request_context_initialized` and switched at runtime with
  `Profile::set_tunnel` (no restart). The offscreen example proves: `10.77.0.x` via the proxy, a runtime
  port switch, and `http://localhost:<port with no listener>/` + its websocket (`CONNECT localhost:<port>`, as
  Vite HMR uses) via the proxy.
  Cost: public traffic takes a local hop through the client proxy, and HTTP/3 isn't used through an HTTP proxy.
- **cef-rs 154.5 pitfalls (audited in `silo-web`):** `None` and `CefString::default()` marshal as null, and
  libcef silently ignores a call with a null required parameter. That includes out-params (`set_preference`'s
  `error`, `Image::as_png`'s sizes) and by-ref structs (`ime_commit_text`/`ime_set_composition`'s
  `replacement_range`, so pass `CefRange::InvalidRange()`). Check `optional_param` in the CEF header before
  passing `None`. `CefStringList::clone` copies the opaque handle by value (dangling), so use `mem::take` on
  borrowed lists. Ref-counted handles (`Browser`, `RequestContext`, …) clone safely (`add_ref`).
- **OSR focus after navigation:** a cross-document navigation creates a new render widget that starts unfocused.
  Blink then drops IME input (not key events), so `silo-web` re-applies `set_focus` on each main-frame
  `on_load_start`.
- **Keychain / cookies at rest (intentional):** the app always passes `--use-mock-keychain`, so Chromium never
  prompts for "Chromium Safe Storage". As a deliberate consequence, cookies, saved logins and other Chromium
  data are stored **unencrypted**. Their only protection is file permissions: `silo-web` creates
  `<root>/profiles` and every profile directory with mode 0700 (re-applied at each start). Anyone running as
  the user, or reading their backups, can read the session cookies. We accept that for a dev tool in exchange
  for no keychain prompt.
- **Tunnel proxy auth:** each client proxy requires Basic proxy auth with per-proxy credentials, passed as
  `silo_web::Tunnel {port, user, pass}` to `runtime.profile(name, Some(tunnel))` / `Profile::set_tunnel`.
  - `RequestHandler::auth_credentials` (IO thread) answers only `is_proxy` challenges whose challenger is
    exactly `127.0.0.1:<that profile's port>`. Origin 401s and other proxies are cancelled, never auto-filled.
  - This requires the `disable-chrome-login-prompt` switch (always set by `silo-web`). Without it, CEF hands
    challenges to Chrome's login dialog, which is invisible in windowless mode, so loads hang.
  - Chromium re-asks right after every rejected 407 (~20/s with stale credentials), so answers are capped at
    4 per second per profile. `set_tunnel` resets the budget.
  - `set_tunnel` on a context that hasn't finished initializing only records the tunnel (CEF rejects prefs
    before init with `has=0 can_set=0`). The init handler then applies the latest one, so an open never fails
    on it.
  - The offscreen example proves: auth with A, a runtime switch to B's credentials, localhost + websocket through
    A, an unanswered origin 401, and wrong credentials stopping after 4 attempts.
- **Helper count and signing:** four helper apps, each signed with entitlements. Notarization time grows.
