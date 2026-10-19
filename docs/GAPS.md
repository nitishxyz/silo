# Gaps: the final batch before Phase 7

Based on HEAD `c92c3e8` and `docs/PLAN.md` §3.6 and §9. Line references are to the working tree at the time
of writing. Rules: this is a dev project with no users, so there are no fallbacks, no compat paths, one clean path,
and a concise UI.

---

## 1. Phase 0 differential fuzz test

**Current state.** `crates/silo-vt/tests/snapshot.rs` has hand-written round trips: a mid-escape continuation
(`:69`), modes/alt screen (`:89`), history interleaved with live output (`:134`), resize (`:164`) and encode time
(`:178`). Nothing feeds random bytes, and no fuzz/differential test exists anywhere (`grep -ri fuzz crates` only
matches `fuzzy`). CI (`.github/workflows/linux.yml`) doesn't test `silo-vt` at all.

**Design.** Add `crates/silo-vt/tests/differential.rs`, with no new dependencies: a 20-line splitmix64 PRNG in the test.
- A generator biased toward the VT grammar, built from weighted pieces: printable ASCII, UTF-8 (including wide/combining
  characters), C0 controls (CR, LF, BS, HT), CSI with random params and finals from `ABCDHJKLMPSTXZdfghlmnqrsu`,
  DEC private modes (`?1049`, `?1`, `?2004`, `?1002`, `?1006`, `?25`), SGR (including 256 and RGB colors), OSC 0/2/7/8
  terminated by BEL or ST, DECSC/DECRC, kitty `>Nu`/`<u`, and raw random bytes (about 5%). Stop each piece at a random
  byte so that snapshots land mid-sequence.
- One case: build `A` at a random grid (20–120 × 5–50) with a random scrollback limit, then feed `N1` random bytes in
  random-sized chunks. Call `bytes = A.snapshot()` and restore `B` with the existing `restore_incrementally`
  pattern. Every third case instead uses head-only restore and interleaves `restore_history` with live writes, as in
  `:134`. Then feed both the same `N2` bytes, with an occasional identical `resize` on both.
- Checks after the restore, after every chunk, and at the end: the `visible()` + cursor + `alt_screen` +
  `mouse_tracking` comparison and the full `dump()` (both helpers move to a shared `tests/common/mod.rs`, so `snapshot.rs`
  uses it too). Also compare `encode_key` for the up arrow and `paste("x")`, which cover the modes.
- On a mismatch, panic with the seed, the case index, the chunk index and a hex dump of the last 64 bytes, so the
  failure is reproducible with `SILO_FUZZ_SEED`.
- Sizes: by default 200 cases × (≤8 KiB before + ≤8 KiB after), with seed `0x5110` plus the case index, which should
  run in a few seconds. A `#[ignore]` long test, `differential_long`, runs `SILO_FUZZ_CASES` cases (default 20 000)
  from a `SILO_FUZZ_SEED` (default: time-based, printed).
- Known divergences are not filtered out. If upstream has a real bug, minimize the case, file it, and pin the
  seed out with a `// upstream: <link>` note. No silent allow-list.

**Files.** `crates/silo-vt/tests/differential.rs` (new), `crates/silo-vt/tests/common/mod.rs` (new, with `dump`,
`visible` and `restore_incrementally` moved out of `snapshot.rs`), `crates/silo-vt/tests/snapshot.rs` (use common),
`.github/workflows/linux.yml` (add `-p silo-vt` to the test step).

**Tests.** The file is the test. Run `cargo test -p silo-vt --test differential` and
`cargo test -p silo-vt --test differential -- --ignored`.

**Findings (T1).**
- Fixed in silo-vt: an OSC 8 URI with invalid UTF-8 (`ESC]8;;\xC1 ESC A \`) made `frame()` fail; link URIs are now
  converted lossily (`sys.rs` `convert_link`).
- Upstream ghostty, open: with a scrollback byte limit below ~16 MiB, reflow resizes prune history differently on a
  restored terminal than on its live source. The snapshot carries the limit and re-applying it on the restored side
  changes nothing; two independent restores agree with each other. Minimal case: 56×43 grid, `scrollback_bytes`
  258 201, write `ESC[65H h ESC[d`, snapshot and restore, resize both to 104×10 then 85×27: live keeps 60 rows, restored
  33. Silo's default (16 MiB) is unaffected. Pinned seeds `0x57c1 0x5a4f 0x5e24 0x5fd1` (`UPSTREAM` in
  `differential.rs`); `byte_limited_history_after_reflow` asserts the divergence and fails once upstream is fixed.
  Random-seed long runs can still hit this class until then.

---

## 2. Phase 2 floating layers

**Current state.** Not implemented. `Layout` (`crates/silo-proto/src/model.rs:124`) is a binary `Block | Split` tree.
`WindowInfo` (`model.rs:247`) has `layout, focused, zoomed`, and the server's `Window` (`silo-server/src/registry.rs:27`)
mirrors it and is persisted as-is in the manifest (`persist.rs:35`). There is no method for floats in `Method`
(`control.rs:81`). The app renders only the tree (`silo-app/src/layout.rs:63` `render_body`).

**Model (proto).** One flat list per window, with no nesting:
```rust
pub struct Float { pub id: BlockId, pub rect: FloatRect }          // block id doubles as float id
pub struct FloatRect { pub x: f32, pub y: f32, pub w: f32, pub h: f32 } // fractions of the window body, 0..1
// WindowInfo / registry::Window gain:
pub floats: Vec<Float>,   // back-to-front; last = topmost
```
- `focused` may name a tiled block or a float.
- `zoomed` applies only to tiled blocks: zooming a float is `bad_request`.
- Fractions keep the layout independent of each client's window size.
- `FloatRect::clamped()` keeps `w,h ∈ [0.15, 1]` and keeps the rect inside `0..1`.

**Server API.** Two methods, plus one changed method:
- `block.float {block_id, rect?}`: moves a tiled block out of the tree into `floats` on top, with a default rect
  centred at 60%×60%. The last tiled block can't float (`conflict`). A float is moved by `block.move` only; there is
  no "float a float".
- `block.move {block_id, rect?, dock?: {target, axis}}` does one of two things:
  - `rect` sets a float's rect and raises it to the top;
  - `dock` removes the block from `floats` and `split`s it next to `target` (this un-floats it).
- `block.focus` on a float also raises it.
- `window.create`/`block.split` are unchanged.
- `remove_block` (`registry.rs:587`) also removes the block from `floats`. A window whose tree is empty but which still
  has floats promotes the topmost float to be the tree root, keeping one clean invariant: a tree always exists.
- `Session::blocks` (`registry.rs:74`) and `window_size` include floats.
- Each mutation bumps the revision and emits `session.changed`.

**App render.** In `render_body`, after the tiled content, paint `w.floats` as absolutely positioned cards
(the same `render_cell` card, with header and shadow-lg) over the body, in z-order. Panes stay keyed by block id in
`workspace.panes`, so floating or docking only moves the element; it doesn't reattach.

**Drag/move/resize.**
- Drag the float's header to move it, and drag its bottom-right corner (a 10 px hit area) to resize.
- The rect is applied optimistically while dragging (`call_optimistic`, as for ratios) and sent as `block.move` on
  release.
- Dropping a float's header on a tiled pane's edge zone docks it there. This reuses the existing `DraggedPane` swap
  machinery (`layout.rs:481`), with an edge zone instead of the centre.

**Focus.** Clicking a float raises it and focuses it. Directional `cmd-alt-arrow` navigation stays tiled-only.
Floats join the reading-order cycle (whatever `cmd-alt-[`/`]` is bound to today; check `commands.rs`) after
the tiled blocks. New palette commands: "Float Pane" and "Dock Pane". No new shortcut.

**Persistence.** Free: `registry::Window` is serialized into the manifest. Add no `#[serde(default)]`: there is
no compat path, so an older manifest fails to load. That is acceptable in dev; call it out in the commit.

**Files.** `silo-proto/src/{model.rs,control.rs}`, `silo-server/src/{registry.rs,conn.rs}`,
`silo-app/src/{layout.rs,commands.rs,palette.rs,floats.rs (new)}`, `silo-cli/src/main.rs` (`silo float <block>`).

**Tests.**
- Proto unit tests: `FloatRect::clamped`.
- Registry unit tests:
  - float, then move, then dock round trip;
  - closing the last tiled block promotes a float;
  - floating the last tiled block is refused;
  - revision and event emitted.
- A `silo-server/tests/persist.rs` case: floats survive a restart.

---

## 3. "Sized by <client>" badge and take size

**Current state: mostly done.**
- Server ownership matches §3.6:
  - owner, candidates and per-client desired sizes: `silo-server/src/block.rs:460-463`;
  - take/request: `:1288-1296`;
  - fallback to the last candidate, else the latest desired size: `:1305-1311`;
  - the theme follows the owner: `:1129`.
- `terminal.size` returns `desired[].name` (`:917`, `:939`).
- The app takes ownership on focus/input and requests it when visible (`silo-app/src/pane.rs:144,335,458`). The CLI
  `attach --watch` uses `Request` (`silo-cli/src/attach.rs:57`).
- The badge exists (`pane.rs:1035-1055`, gated by `sized_elsewhere` at `:852`), but it reads "sized by another client · take
  size". The owner's name is never shown, because `BlockInfo` (`silo-proto/src/model.rs:96`) carries `owner:
  Option<ClientId>` and no name.
- Non-owner rendering: the canvas paints the owner's grid from the top-left, clipped by the pane mask
  (`silo-render/src/surface.rs:157`). A smaller grid leaves bare pane background, which looks like a letterbox, but
  without any visual edge.

**Design.**
- Proto: add `owner_name: Option<String>` to `BlockInfo`, filled in `publish` (`block.rs:1402`) from the desired entry's
  `name`. Show a short name: `client_name` up to the first ` (`, so "Silo 0.1.0" becomes "Silo", and the CLI is "silo cli".
  Add the host part for remote peers by appending the paired device name the server already knows (`registry.client_name`,
  `registry.rs:223`).
- Badge text: `sized by {short}` plus a separate `Take size` link. It sits top-right and is muted. Clicking only the
  link calls `own(Take)`, and the whole badge disappears once the next `block.changed` shows us as owner (as today).
- Letterbox: when the applied grid is smaller than the pane, draw a 1 px `theme::border()` rule along the grid's
  right and bottom edges, and fill the outside with `theme::surface()` so it reads as "not ours". When it is larger, it
  stays clipped as today. No scaling.

**Files.** `silo-proto/src/model.rs`, `silo-server/src/block.rs`, `silo-app/src/pane.rs`.

**Tests.**
- A server test in `silo-server/tests/stream.rs`: two clients, B takes ownership, A's `BlockInfo.owner_name` is B's
  short name, and after B disconnects ownership falls back to A.
- A pane unit test of the short-name helper.

---

## 4. Heartbeat and Offline detection

**Current state.** None. There is no ping method, no keepalive and no call timeout:
- `Control::call_value` (`silo-client/src/control.rs:247`) awaits forever;
- the app's connection loop (`silo-app/src/conn.rs:186-212`) only reacts to the event stream ending.

A server that is hung (deadlocked on `registry.lock()`, SIGSTOPped, a stalled iroh path) keeps the socket open, so the
app never leaves "connected". `Offline` (`workspace.rs:36,339`) appears only after a close. The CLI is the same.

**Design.**
- Proto: add `Method::Ping {}`, which returns `{}` (`control.rs:81`, `METHODS`). In `silo-server/src/conn.rs`, add
  it to `read_only` (`:221`). `handle` takes and drops `server.registry.lock()` before replying, so a deadlocked
  registry reads as Offline instead of being hidden by a ping that never touches shared state.
- Client: add `Control::call_timeout(method, dur)` (a `tokio::time::timeout` around `call_value`, which removes the pending id
  on expiry) and `Control::heartbeat(self) -> impl Future<Output = ()>`, which resolves when the server stops
  answering. It pings every `PING_EVERY = 5 s`, and if `PING_TIMEOUT` is exceeded twice in a row it closes the
  connection (marks it closed and drops the writer). Timeouts: 3 s for local, 10 s for iroh (`Conn::Iroh`).
- App: in `conn.rs` `run`, add a `heartbeat` arm to the inner `select!`. When it fires, break with reason
  `"server not responding"`, which goes through the existing `Disconnected` → `OFFLINE` path and the reconnect loop.
  Panes already hide their own status while the control is closed (`pane.rs:837`).
- CLI attach: run the same heartbeat alongside `attach::run`. On expiry print `silo: server not responding` and exit 1.
- The menubar (`silo-menubar/src/net.rs`) gets the same heartbeat, so it reports the server down. Check how it
  shows status today before wiring it in.

**Files.** `silo-proto/src/control.rs`, `silo-server/src/conn.rs`, `silo-client/src/control.rs`,
`silo-app/src/conn.rs`, `silo-cli/src/attach.rs`, `silo-menubar/src/net.rs`.

**Tests.** A `silo-client/tests/e2e.rs` case: start a server, connect, and `kill -STOP` the server pid. The heartbeat
resolves within `2 × timeout + interval`. Then `kill -CONT`, and a reconnect succeeds. Also a unit test that
`call_timeout` cleans up its pending entry.

---

## 5. UI review list

**5a. The status message resizes every terminal by a row.**
- Cause: the status bar is a 22 px flex child *below* the body (`workspace.rs:1075-1088`), so showing it shrinks
  `flex_1` (`:1074`), and every pane's grid loses a row and resizes the PTY.
- Fix: overlay it instead. Make it an absolute toast pinned bottom-centre (`.absolute().bottom(px(8)).left/right`,
  centred, `continuous(…)`, `bg(popover)`, a border, 11 px muted text) that never takes layout space.
- Drop the corner special-case `self.status_bar().is_none()` at `layout.rs:68` and `:367`, so the corners stay constant.

**5b. Escape doesn't close the eye menu.**
- Cause: `appearance.rs` only closes the menu on `on_mouse_down_out` (`:98`). The menu has no focus or key handling,
  and focus stays in the terminal, so the pane consumes Escape.
- Fix: give `AppearanceMenu` a `FocusHandle` and focus it in `toggle_appearance_menu` (`:26`) when opening. Add
  `.track_focus(...)` and `.on_key_down` on `"escape"` to the menu, which closes it and calls `refocus_terminal`.
  Also close it when the menu loses focus. Add `!self.appearance_menu.open` to both auto-refocus guards in
  `Workspace::render` (`workspace.rs:990-1006`), so the terminal doesn't steal focus back.

**5c. Tab titles show long paths; visibility.**
- Cause: `pane::block_title` (`pane.rs:871`) returns the whole `~/a/b/c/d` cwd for a shell. Tabs are ellipsized at the
  *end* (`titlebar.rs:417`, `TAB_MAX` 190 at `:34`), so the useful last component is the part that gets cut.
- Fix: for a shell, show the last path component only (`~` for home, `/` for root). The full path stays in the pane
  header (`layout.rs:491` `header_labels`).
- Visibility: inactive tabs use `theme::muted()` (`titlebar.rs:459`) on `tab_hover_on(strip)` backgrounds. Raise the
  inactive text to `foreground` at 0.7 opacity, matching the lifecycle badge (`pane.rs:1021`), and keep muted for icons only.
- Test: a unit test for `block_title` covering home, nested, root and non-shell titles.

**5d. Font picker placement.**
- Current state: the menu is anchored `TopRight` under the trigger (`settings_panel.rs:1229-1232`, `top(px(32)).right_0()`),
  300 px wide (`:1206`), while the trigger is 196–260 px wide and right-aligned in the row. The menu therefore overhangs
  to the left, toward the label column, and near the bottom of the page it covers the trigger.
- Fix: anchor `TopLeft` at the trigger's left edge, with the menu width = `max(trigger width, 260)`. Use GPUI's
  `anchored().position_mode(..)` with the trigger bounds captured by an `on_children_prepainted`/canvas, so it flips
  above the trigger when there is no room below (`snap_to_window_with_margin` alone only shifts it).

**5e. 123 font warnings logged each time Settings opens.**
- Cause: `open_settings` (`settings.rs:218`) calls `theme::refresh_fonts`. When the family list differs at all (and on
  the first open), it resets the catalog (`theme.rs:228-230`). Then `monospace_fonts`/`ui_fonts` (`theme.rs:246,281`)
  call `text.resolve_font` for *every* installed family. GPUI's `load_family` (`gpui_macos/src/text_system.rs:313-370`
  in the pinned zed) `log::warn!`s for each face without an `m` glyph, without a PostScript name, or a duplicate.
  The settings list also renders each entry in its own face (`settings_panel.rs:1187`), which triggers the same loads.
- Fix:
  - compute both catalogs once per app run on a background executor;
  - `refresh_fonts` only re-probes *added* names (diff against the cached set);
  - keep a process-wide `rejected: HashSet<String>` of families whose resolve fell back, and skip them on later
    probes and in the list's per-row `font_family` (render rejected ones in the UI font, or drop them from the list,
    since they're unusable anyway);
  - in `main.rs`, set the env-filter default to `gpui_macos::text_system=error`, which silences the remaining one-time
    warnings at the source.
- Test: a pure helper `fonts::to_probe(cached, installed) -> Vec<String>` with a unit test.

**5f. ⌘9 should select the last tab.**
- Current state: `SelectWindow9` calls `number_key(8)` (`workspace.rs:1072`), and `select_window(8)` (`:931`) is the 9th tab.
- Fix: `number_key` maps index 8 to `windows.len() - 1` (Safari/Chrome convention). The switcher keeps ⌘9 as row 9.
- Verify the cause first: run with `RUST_LOG=gpui_macos=warn`, open Settings twice, and confirm whether the second
  open still warns. If it does, the per-row `font_family` render (`settings_panel.rs:1187`) is the culprit, not the
  catalog, and the `rejected` filter is the fix.

**5g. Settings-file row placement.**
- Current state: "Settings file / terminal.json" with Show in Finder / Edit / Reload is the *last* group on the Terminal
  page (`settings_panel.rs:1356-1371`), below "Restore defaults" (`:1345`). Three buttons squeeze the row when narrow.
- Fix: move it to the top of the Terminal page, as a single compact header line under the preview: `terminal.json`
  in muted mono, plus icon buttons (reveal, edit, reload) with tooltips. "Restore defaults" stays last, as the
  destructive action at the bottom.

**5h. A v1 "Menlo" font setting loads as Automatic: drop it.**
- `TerminalPrefs::validated` rewrites `version < 2 && font_family == "Menlo"` to Automatic (`terminal_prefs.rs:228-232`),
  with test `version_one_menlo_default_becomes_automatic` (`:492-507`). This is exactly a compat path.
- Fix: delete the branch and the test, and drop the `version` field and `VERSION` entirely (`:20`, `:183`, `:198`,
  `:225-233`), including the "written by a newer Silo" problem. There are no users; an explicit "Menlo" now means Menlo.
- Also remove the version comparison in the remaining tests (`:486`, `:499`).

---

## 6. Other PLAN Phase 0–6 items still open

These were confirmed by grepping the code; Phase 7 and iOS are excluded.

| PLAN item | State | Action |
|---|---|---|
| §3.2 / Phase 2 "schema'd API": `api list/describe/call`, JSON Schema from `schemars` | Only `api.list` with names and one-line descriptions (`control.rs:232`). `schemars` is not a dependency of `silo-proto`, and there is no `api.describe` | **Do it (Task 5).** Derive `JsonSchema` on `Method` params and results in `silo-proto`. Add `api.describe {method}` → `{description, params_schema, result_schema, example}` and the CLI `silo api describe <m>`. Test: every `METHODS` entry describes, and its schema validates its example. |
| §3.6 badge names the owner | See gap 3 | Task 3 |
| §4 Ohara HMAC adoption challenge | Covered differently: `ClientHello.token`, a shared secret (`control.rs:17`), plus a peer-uid check (`silo-server/src/conn.rs`) | None. One mechanism is enough; don't add HMAC. |
| Phase 4 "relay config" in the multi-server UI | Server side done: `silo server --relay/--no-mdns` (`silo-cli/src/main.rs:48-56`). The app hardcodes `RelayConfig::Default, mdns: true` (`silo-app/src/remote.rs:27`) | **Drop.** The defaults are right for a client, and the server is configured where it runs. |
| §5 deb/rpm/tarball, the xtask sign/notarize | Linux and macOS have tarballs from the release workflow (`.github/workflows/release.yml`), installed by `install.sh` and run as a user service by `silo up`; macOS uses `scripts/bundle-macos.sh`; there is no `xtask` | **Defer**, not a gap for a dev project with no users. Release work. |
| Phase 0 fuzz in CI | `silo-vt` isn't in `linux.yml` | Task 1 |
| Phases 3, 5, 6 (resume, skip-ahead, cache, persistence, hot upgrade, prediction, latency bench, browser, tunnels) | Present: `block.rs` skip-ahead, `silo-client/tests/resume.rs`, `persist.rs`, `handoff.rs`, `predict.rs`, `silo-latency`, `browser*.rs`, `tunnel.rs` | None |

---

## Implementation split

Five tasks. Each file has exactly one owner. Order: T1 is independent; T2 → T3 → (T4 ∥ T5). T4 and T5 both
consume the T2 proto types and touch no shared files.

**T1 — Differential fuzz (gap 1).**
Files: `crates/silo-vt/tests/{differential.rs,common/mod.rs,snapshot.rs}`, `.github/workflows/linux.yml`.
Acceptance:
- `cargo test -p silo-vt` passes with the 200-case default in under 10 s (release deps).
- The `-- --ignored` long mode honours `SILO_FUZZ_CASES`/`SILO_FUZZ_SEED`.
- A failure prints the seed and case.
- CI runs `-p silo-vt`.

**T2 — Protocol and server: floats, owner name, ping, schemas (gaps 2, 3, 4 and 6 server side).**
Files: `crates/silo-proto/**` (+ `schemars` dependency), `crates/silo-server/**`.
Acceptance:
- `Float`/`FloatRect` on `WindowInfo` and `registry::Window`; `block.float`, `block.move` (rect | dock); float-aware
  `focus`, `remove_block`, `blocks()`.
- `BlockInfo.owner_name` (short name).
- `ping` takes the registry lock.
- `api.describe` with schema and example for every method.
- Registry unit tests, persist test, two-client ownership test and describe test pass:
  `cargo test -p silo-proto -p silo-server`.

**T3 — Client, CLI and menubar: heartbeat, CLI verbs (gaps 4 and 6 CLI side, 2 CLI).**
Files: `crates/silo-client/**`, `crates/silo-cli/**`, `crates/silo-menubar/**`.
Acceptance:
- `Control::call_timeout`/`heartbeat` (3 s local / 10 s iroh, two misses).
- The SIGSTOP e2e test passes.
- `silo attach` exits 1 with "server not responding" on a hung server.
- `silo float <block>` and `silo api describe <method>` work.
- `cargo test -p silo-client -p silo-cli` passes.

**T4 — App polish, badge, heartbeat arm (gaps 3 app side, 4 app side, 5a–5h).**
Files: `crates/silo-app/src/{workspace.rs,titlebar.rs,appearance.rs,settings.rs,settings_panel.rs,theme.rs,fonts.rs,
terminal_prefs.rs,main.rs,pane.rs,conn.rs}`.
Acceptance:
- The status toast overlays the content: no PTY resize when it appears (`terminal.size` is unchanged).
- Escape closes the eye menu and refocuses the terminal.
- Shell tabs show the last cwd component, and inactive tab text is legible.
- The font menu is left-anchored and flips above the trigger near the bottom.
- Opening Settings a second time logs no font warnings.
- ⌘9 selects the last tab.
- The terminal.json line sits at the top of the Terminal page.
- No `version` field remains in `terminal_prefs`.
- The badge reads "sized by Silo · Take size" with the owner's short name, and the letterbox edge is drawn.
- A hung server shows Offline within about 10 s locally.
- `cargo test -p silo-app` passes; a manual UI pass covers each item.

**T5 — App floating layers (gap 2 app side).**
Files: `crates/silo-app/src/{layout.rs,commands.rs,palette.rs}`, plus a new `crates/silo-app/src/floats.rs`
(drag/resize/dock handlers as `impl Workspace`). It also owns the 5a corner cleanup at `layout.rs:68,367`.
Acceptance:
- Float Pane / Dock Pane from the palette.
- A float renders above the tiles in z-order, and click raises and focuses it.
- Header drag moves and corner drag resizes it optimistically, with one `block.move` sent on release.
- Dropping it on a pane edge docks it.
- Floats survive an app restart and a server restart.
- Window corners no longer depend on `status_bar()`.

---

## Recommended drops

- **5h compat migration**: delete it, together with the prefs `version` field (no users, no compat).
- **Relay config in the app UI** (Phase 4): the defaults are correct, and the server is configured via the CLI.
- **HMAC adoption challenge** (§4): already covered by the hello token plus the peer-uid check.
- **deb/rpm/xtask notarization** (§5): release work, out of scope for this batch.
