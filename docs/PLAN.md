# Plan: a block multiplexer (server, client, sync)

Status: decisions locked for v1 (section 10). Sources: upstream Ghostty `main` (`d2865b8f`, 2026-10-18), `desktop-platform`
(libghostty-vt terminal + CEF browser, Linux), `torrz`/Ohara (GPUI fork, bundled daemon, iroh 1.3).

---

## 1. Prior art: snapshot-based attach

The reference design is an existing server/client terminal multiplexer:
- A native client rendering with libghostty, and one server binary that is also the CLI.
- Runtime: a unix socket `server.sock` in the app support directory, plus `servers/<id>/runtime.json`
  (`{endpoint, pid, instance_id}`) and `start.lock`.
- Per-terminal **encrypted snapshot files** (`terminal/<server-pid>/<uuid>.snap`), most likely libghostty
  snapshots kept for recovery or restart.

### Snapshot handoff: done by upstream libghostty, nothing custom

That server links these exported symbols: `ghostty_snapshot_encode*`, `ghostty_snapshot_decoder_{new,ready,next,decode}`,
`ghostty_terminal_continuation_*`, `ghostty_terminal_vt_write_until_ground`, and `ghostty_terminal_compress`. They
are all **public upstream API** (`include/ghostty/vt/snapshot.h`, `src/terminal/snapshot/`).

The snapshot format, which is also the attach flow:

```
GHOSTSNP v1 | TERMINAL | SCREEN+PAGEs (active area) | CONTINUATION | READY | HISTORY+PAGEs (newest->oldest) | FINISH
```

- Every record is CRC32C-protected.
- **TERMINAL** carries cols/rows/px, scroll region, current/saved/default modes, mouse event/format/shape,
  modify-other-keys, the password-input flag, cursor defaults, dynamic colors, the palette and its overrides,
  tab stops, pwd, title and scrollback limits.
- **SCREEN** carries the cursor, saved cursor, charsets G0–G3, protected mode, the **kitty keyboard flag stack**
  and hyperlinks. Primary and alternate screens are both included.
- **CONTINUATION** carries the unfinished VT/UTF-8 parser input, so a snapshot taken in the middle of an escape
  sequence still restores exactly.
- `decoder_ready()` returns a renderable terminal at READY. After that, `decoder_next()` prepends one history page
  at a time. `history_rows` is known at READY, so the scrollbar is sized correctly straight away.
- The `COMPRESS_HISTORY` decoder option compresses scrollback as it restores.
- Ghostty's docs say live PTY data must be **multiplexed outside** the snapshot record sequence, which is the
  transport's job.
- Caveat: "format version 1 is WIP, no binary-compat guarantee". So the client and server must agree on the Ghostty
  revision (see §3.5).
- The Ghostty commit desktop-platform pins (`f9a3f24a`) already has `snapshot.h`. Current `main` adds roughly 270
  lines, including `COMPRESS_HISTORY`. **Bump the pin to current main** at project start.

The reference server likely also embeds a snapshot in-band in a VT stream, inside an authenticated APC
sequence, for CLI or "dumb terminal" attach. That's an inference, not verified.

### Terminal size: block ownership

- Each block has **at most one owner client**: `session.own_block {block_ids?, mode: "take"|"request"}` and
  `session.disown_block`.
- Every client sends its desired size. The server records them per client with timestamps:
  `terminal.size` returns `{columns, rows, cell_width_px, cell_height_px, owner, desired:[{client_id, columns, rows, updated_at}]}`.
- `terminal.resize`: "applied if the caller owns the block or the block has no owner. Otherwise it is remembered
  and applied when the caller becomes the owner."
- **Theme follows the owner too.** `set_theme` is remembered per client, and the owner's theme is used (any
  client's while the block is unowned).
- A client that disowns a block it doesn't own "only stops being a candidate". So there is a candidate list, and
  ownership falls back to another candidate.

### Control and data planes (verified)

- Control: JSON methods, each with a JSON Schema input/output, examples and validation notes. The API describes
  itself (`help.all`, `api list/describe/call`). The CLI and Lua are generated from it.
- Events: `seq`-numbered with no gaps. If an event backlog exceeds **8 MiB, the server closes the connection
  rather than drop events**. The client reconciles a snapshot plus events using `seq`.
- Structural mutations return a session `revision`.
- **Data plane is separate.** `data.connect {session_id, block_id, data_ref_id:"terminal", subprotocol:"silo.data.terminal.v1"}`
  returns a **single-use 16-byte ticket** (60 s, bound to the caller's principal). The client then opens a data
  connection: `HELLO(ticket)` → `WELCOME`, or `REJECT(reason)` (0x07 means subprotocol mismatch).
- `terminal.reset` notes: the server "waits up to 250ms for a **parser-ground boundary** so the same bytes follow
  earlier output". This is how out-of-band changes are spliced into the byte stream every client sees.

---

## 2. Our architecture

```
                         ┌──────────────── server (daemon, always on) ────────────────┐
                         │  session registry  (sessions/windows/layers/blocks, rev'd)  │
  GPUI client ──┐        │  block: terminal                                            │
  CLI client  ──┼─ conn ─┤    PTY actor ─ raw byte ring (monotonic offsets) ─┐  tee    │
  iOS (later) ──┘        │    libghostty-vt server copy (snapshot/status)    ▼         │
                         │  block: browser (state + tunnels)                           │
                         │  event bus (seq) · control API (schema'd) · auth/pairing    │
                         └─────────────────────────────────────────────────────────────┘
   conn = unix socket (local) | iroh QUIC (remote). One control stream + one data stream per attached block.
```

- **Client (per terminal block):**
  - Its own libghostty-vt, restored from a snapshot and then fed the same raw bytes.
  - Local renderer: desktop-platform's single-canvas GPUI painter.
  - Local key/mouse encoding, viewport, selection, search, IME and cursor blink.
- **Server (per terminal block):**
  - A PTY actor that exclusively owns the PTY and the authoritative libghostty-vt, with continuation tracking on.
  - A raw output ring with byte offsets, per-client sizes, ownership, and events (title/pwd/bell/OSC 7501/exit).

---

## 3. Sync protocol

### 3.1 Transport framing

Every stream (unix or iroh QUIC) carries length-prefixed frames: `u32 len | u8 kind | payload`.
- Control payloads are JSON.
- Data payloads are binary; snapshot bytes pass through untouched.

### 3.2 Control plane: decided

- JSON-RPC-style frames: `{id, method, params}` → `{id, result|error{code,message}}`, plus `{event, seq, payload}`.
- The method registry is Rust types with `serde` + `schemars`. That gives us `api list/describe/call`, JSON Schema
  and examples for free, and the CLI, Lua and agent tooling are generated from it.
- Events carry `seq` with no gaps. If the per-client queue exceeds 8 MiB, the server closes the connection; the
  client reconnects and calls `session.view` again.
- Mutations return `revision`.
- Error codes: `bad_request`, `not_found`, `method_not_found`, `conflict`, `unauthorized`, `timeout`.
- **Why not Ohara's HTTP-over-iroh:**
  - We need server push on the same connection, and a per-connection identity that controls attach and data tickets.
  - The same frames work on a unix socket and on a QUIC stream with no hyper/axum layer.
- A WebSocket listener carrying the same frames can be added later for a web client.

### 3.3 Data plane (one stream per attached block)

1. Control: `data.connect {block_id, subprotocol:"term/1"}` → `{ticket[16], expires_in_ms}`.
2. Open a new stream (a new unix connection, or a new iroh bi stream on the same connection).
3. Client → `Hello {ticket, ghostty_rev, snapshot_version, resume?: {epoch, offset}, cols, rows, cell_px}`
4. Server → `Welcome {epoch}` or `Reject {reason}`.

| Client → server | Server → client |
|---|---|
| `Input {bytes}` | `SnapshotChunk {bytes}` (raw GHOSTSNP stream, split into frames) |
| `Resize {cols, rows, cell_px}` | `Output {offset, bytes}` |
| `Ack {offset}` | `SizeChanged {cols, rows, owner}` |
| | `Exited {code}` |

Clipboard layers: remote clients send `ClipboardPush` before a paste so programs that read the host's clipboard directly see it (NSPasteboard on macOS, the real `wl-copy`/`xclip` with a display), and macOS hosts send their own clipboard changes back to remote clients. Headless hosts have no system clipboard: the `pbpaste`/`xclip`/... shims in `<SILO_HOME>/shims` ask clients through `clipboard.read`/`clipboard.write`. OSC 52 always reaches every client.

### 3.4 Attach algorithm (server side, inside the PTY actor so nothing races)

1. **Resume** if `resume.epoch == epoch` and `resume.offset >= ring.start`: send `Output` from that offset. No
   snapshot is needed (laptop sleep, network blip).
2. **Otherwise snapshot:**
   - Between reads, set `base = ring.end`.
   - `ghostty_snapshot_encode` streams into `SnapshotChunk`s, in a background task that writes from a terminal copy
     if encoding history turns out slow (verify in Phase 0).
   - Any new PTY output queues as `Output{offset >= base}`.
   - The client decodes until `ready()`, applies `Output` frames from `base` in order, and renders.
   - It then keeps calling `next()` to prepend history pages while live output continues. Ghostty supports this
     "ready first, then incrementally prepend" mode; confirm it interleaves with `vt_write` (Phase 0 test).
3. **Fall behind:** if a client's unacked window passes the ring start (slow link, `cat huge.log`), the server
   drops its backlog and re-snapshots (Mosh-style skip-ahead). Fast clients are never slowed by slow ones.
4. **Out-of-band changes** (reset, theme) are applied only at a **parser-ground boundary**
   (`ghostty_terminal_vt_write_until_ground`), so every client sees the same byte order.

### 3.5 Version skew

No version numbers and no fallbacks. Every hello carries `BUILD`, a compile-time hash of the workspace sources,
`Cargo.lock` and `GHOSTTY_REV`.
- **Same build:** connect; snapshots are always native.
- **Different build:** the server rejects the hello. The app shows "Different server build" with **Upgrade Server**,
  which hot-upgrades the running server in place (§5), keeping every session.

### 3.6 Terminal size and multiple clients: decided (block ownership)

- Each block has an owner client. Every client always reports its desired size and theme; the server stores the
  latest per client.
- The applied PTY size and theme are the owner's. If the block is unowned, the latest desired size is applied.
- Ownership:
  - **The GPUI app takes ownership** (`own_block mode=take`) of the blocks it displays when the user focuses or
    types in them. That's the "latest-active wins" behaviour, made explicit.
  - **Passive clients** (CLI `attach --watch`, a phone viewer, an agent) use `mode=request`: they only get ownership
    if the block has none.
  - **When the owner disconnects or disowns**, ownership goes to the most recently active remaining candidate.
- Non-owners render the owner's grid at their own font size: clipped or letterboxed, with a subtle "sized by
  <client>" badge and a one-click "take size".
- Reason: one PTY can only have one size, and tmux's "smallest wins" punishes the big screen. Explicit ownership
  is predictable and also settles the theme question.

### 3.7 Persistence

- **Session tree:** saved on the server (sqlite or JSON). On server restart, sessions and layouts come back, but
  processes can't.
- **Terminal contents:** a periodic `ghostty_snapshot_encode` of each terminal to disk, encrypted at rest with a
  per-host key.
  - After a crash or reboot, a block reopens showing its old screen and scrollback ("restored, process exited").
  - The same files serve hot upgrades.
- **Client:** a disk cache per server (session tree plus last snapshot per block), for instant UI on launch and
  offline display.

---

## 4. Transport, identity, auth

- **Local:** a unix socket at `~/Library/Application Support/<name>/server.sock` (macOS) or
  `$XDG_RUNTIME_DIR/<name>/server.sock`, mode 0600.
  - The peer uid is checked with `getpeereid`/`SO_PEERCRED`, and the principal is recorded as `uid N over unix`.
  - `runtime.json` + `start.lock` for discovery (the Ohara pattern); adoption is checked with Ohara's
    HMAC challenge.
- **Remote:** iroh `=1.3.0` (newest release; MSRV 1.91, so `silo-transport` sets `rust-version = "1.91"`),
  in the `silo-transport` crate. Server identity is a persistent EndpointId whose secret lives in
  `<home>/state/iroh/key` (same 0600/no-symlink rules as the snapshot key, separate file). One endpoint per
  process; the server runs one `Router` with ALPNs:
  - `silo/ctl/1`: the control stream is the first bi stream. Data streams are opened as extra bi streams on the
    same connection, each starting with `Hello(ticket)`. A QUIC stream is only visible to the peer after its
    first write, so the opener always speaks first.
  - `silo/tunnel/1`: port forwarding for browser blocks (reserved, not served yet).
  - Relays: n0 by default (plus n0 DNS/pkarr lookup), one custom relay URL, or disabled (direct/mDNS only).
    mDNS on the LAN is a separate on/off flag. Multipath handles Wi-Fi ↔ cellular.
- **Pairing:** Ohara-style tickets, bound to `connection.remote_id()`, with owner vs paired-device permissions.
  The invite string is `silo1` + lowercase unpadded base32 of postcard `{EndpointAddr, secret: [u8; 32]}`;
  malformed strings are rejected.
- **Machine linking:** both flows end in the `host.offered` event (`HostOffer {ticket, name, endpoint_id,
  source}`), sent to local (owner) clients only since it carries a pairing ticket; the app turns it into
  "add this host".
  - SSH auto-detect: shells get `LC_TERMINAL=silo` (ssh forwards `LC_*`). A `silo` on the far side prints the
    private OSC `ESC ] 7717 ; offer ; <silo1 ticket> ; <hostname> (BEL | ST)` (`silo_proto::offer::osc`). The
    server's block actor strips it from PTY output (`OfferScanner`, split-safe, 2 KiB cap) and raises valid
    tickets with `source: terminal {block_id}`, each ticket once per block, at most 3 per minute.
  - Join (reverse pairing): `pair.create {purpose: "join"}` mints an invite that pairs nothing; the redeeming
    connection may only call `host.offer {ticket, name}` once (`silo_client::offer_host`), raising
    `source: join {invite_id}` (`PairInvite.invite_id` lets the inviter recognise its own invite).
- **Remote build mismatch:** a remote server with a different `BUILD` is shown as "Different server build"
  with no Upgrade button. It is updated on its own machine by running the new CLI there (CLI takeover, §5).
- **Later:** SSH-key auth, and a login server that maps identities to unix accounts.

---

## 5. Daemon lifecycle and packaging

- The server crate builds both a library and the `<name>` binary, which is server and CLI in one.
- **macOS app:**
  - Bundles it. Under launchd the app copies the server and menu-bar agent to `<SILO_HOME>/runtime/<build>/`
    (`silo`, `silo-menubar`) and the agent runs them from there, so nothing holds Silo.app open and Finder can
    replace it while the server runs; moving to the new build is a hot upgrade. Once this build's job serves, runtime
    dirs of other builds that no process runs from and nothing touched in 5 minutes are removed (under
    `runtime.lock`). A new menu-bar agent replaces the one `menubar.lock` records if that is of another build.
  - **Launch = `silo up` (done):** a release Silo.app in (~)/Applications, on a disk image or translocated, with the
    default state dir installs or refreshes the launchd agent for its runtime `silo` (shared `silo_client::service`),
    taking over or adopting a running server live, points `<SILO_HOME>/bin/silo` at that `silo` and links
    `~/.local/bin/silo` to it when that dir exists. Shells of a runtime-dir server get `<SILO_HOME>/bin` on `PATH`.
    Anything else (`SILO_HOME`, dev and debug builds, a bundle in a cargo target dir) starts a plain server; Stop
    disables the agent until the next launch.
- **Server modes:**
  - Personal: auto-exits about 10 s after the last session and client, unless started explicitly.
  - Shared and login modes come later.
- **Hot upgrade:** `silo server --takeover` connects to the running server with the fixed line `SILO-UPGRADE 1`.
  The old server freezes every block, checkpoints it at an exact byte boundary, and passes the tree, checkpoints,
  PTY masters, lock and listener over `SCM_RIGHTS`. The successor adopts them (same PIDs, same epochs and offsets,
  so clients resume without a gap) and replies `READY`. Before `READY` any failure aborts and the old server
  re-adopts everything; shells are never hung up.
- Packaging, signing and notarization reuse Ohara's xtask. Linux uses systemd units plus deb/rpm/tarball.
- **Install and service (done):** the release workflow publishes `silo-<os>-<arch>.tar.gz`; `install.sh` installs it
  to `~/.local/bin` and runs `silo up`. `silo up`/`down`/`status` manage the server as a user service (systemd user
  unit or launchd agent, generated by the CLI; `server --adopt` takes over a running server); re-running `up` after
  installing a new build upgrades live.
- **Linking machines (done):** `silo join <ticket>` offers this machine's server to the one that made the join
  invite; `silo remote install <ssh-target>` installs silo over SSH, joins and adds the host; `silo up` over SSH from
  a Silo terminal offers the machine inline (OSC 7717).

---

## 6. Browser blocks

- **Default:**
  - The engine runs on the client: CEF windowless, composited into GPUI. On macOS, CEF's accelerated paint gives an
    **IOSurface**; wrap it as a CVPixelBuffer and use the fork's existing
    `2b9064d7 Draw 32BGRA CVPixelBuffer surfaces via CVMetalTextureCache`.
  - Block state lives on the server: URL, history, title, favicon, zoom, profile.
  - `localhost`/private-network requests are tunneled over iroh `tunnel/1` to the server machine.
- **Later:** a server-side headless Chromium flavor driven over CDP, for agents.
- **Port from desktop-platform:** `web-runtime`/`webview`. On macOS, swap the Linux runtime script for a CEF macOS
  distribution and DMA-BUF for IOSurface, and add an IME bridge (missing there).

---

## 7. Reuse

| From | Reuse |
|---|---|
| Ghostty upstream | libghostty-vt incl. **snapshot encode/decode, continuation, write-until-ground, compress** |
| desktop-platform `terminal-vt` | Zig build script, `build.rs` static link, C adapter + Rust FFI. **Extend the adapter with snapshot/continuation**, or switch to the `libghostty-vt` crate (0.2.2) if it covers them |
| desktop-platform `terminal-session` | PTY actor, bounded queues, launcher self-exec. **macOS port:** kqueue `EVFILT_PROC` instead of pidfd, `posix_openpt` etc. |
| desktop-platform `terminal-renderer`/`terminal-ui` | GPUI single-canvas painter, background shaping, IME bridge |
| desktop-platform `web-runtime`/`webview` | CEF sessions, frame pump, input (port to macOS) |
| torrz/Ohara | crate split, `ServerConnection` trait idea, discovery/HMAC, iroh pairing + Router, xtask packaging |

**GPUI pin: decided.** Use `nitishxyz/zed@b36b2e8276c7` (current `torrz/glass` head). desktop-platform's
`e429a0bf` is its direct ancestor (6 commits behind), so **no merge is needed**. The 6 newer commits add Metal
backdrop blur, squircle corners, paint-only scale, macOS 26 blur, bicubic sampling, and CVPixelBuffer surfaces
(needed for CEF on macOS). Track `torrz/glass` by rev.

---

## 8. Workspace layout

```
crates/
  proto        control method types (+schemars), events, data frames, tickets
  transport    Conn/Stream traits; unix impl; iroh impl; pairing
  vt           libghostty-vt build + adapter (+ snapshot/continuation) + safe API
  pty          PTY open, launcher, child supervision (macOS kqueue, Linux pidfd)
  server       registry, terminal block (ring, attach, ownership), browser block, events, persistence
  client       multi-server manager, attach/resume, snapshot restore, cache
  term-ui      renderer + GPUI terminal surface (+ prediction overlay later)
  web          CEF runtime + GPUI webview
  app          GPUI app
  cli          CLI (shares binary with server)
xtask          build ghostty, package, sign/notarize, smoke tests
```

Window material (`app/src/material.rs`): Solid, or Frosted (compositor blur behind veils). Frosted renders only where
`blur_supported()` holds and transparency is not reduced; otherwise the window is solid and the choice is kept. macOS
always qualifies, falling back while Accessibility › Reduce transparency is on (observed via NSWorkspace). Windows 11
(build 22000+) and blurring Linux compositors (KDE) qualify once the GUI runs there; until then they report unsupported.
Frosted is one material: titlebar, gutters and every card are one veil at one per-frame alpha (never stacked), raised
until all text, the terminal foreground included, reads at 4.5:1 over the theme background, black and (lifted) white.

---

## 9. Phases

**Phase 0 — spikes (macOS)**
- Build libghostty-vt at the current Ghostty main with Zig 0.16. Port `terminal-vt` and `pty` to macOS.
- Snapshot round trip in Rust, ported from `example/c-vt-snapshot`. Checks:
  - snapshot taken mid-escape-sequence;
  - alt screen + kitty keyboard + mouse modes;
  - incremental history prepend **interleaved with live `vt_write`**;
  - encode time for 10k/100k lines.
- Differential fuzz test (`A` → snapshot → `B`, keep feeding both; render frames must match). This gates Phase 1.
- GPUI app skeleton on `b36b2e82` with the terminal canvas.

**Phase 1 — local, one terminal block**
- Daemon, unix socket, adopt/spawn.
- Control: `session.create/list/view`, `data.connect`. Data: hello/snapshot/output/input/resize.
- Quit the app, reopen it, get the same shell back.

**Phase 2 — the multiplexer**
- Windows (tabs), split layouts, floating layers, zoom, labels, focus.
- Revisioned tree + seq events + schema'd API + CLI (`ls/new/attach/send/capture/api/events`).
- Ownership and size policy.

**Phase 3 — robustness**
- Offset resume, skip-ahead, multiple clients, client cache.
- On-disk encrypted snapshots, restore after crash, hot upgrade.

**Phase 4 — remote**
- iroh `=1.3.0` transport + identity (`silo-transport`), pairing, multi-server UI, relay config,
  Linux server + systemd.
- Remote build mismatch: "Different server build", no Upgrade button; update via the server's own CLI takeover.

**Phase 5 — latency**
- Predictive local echo (Mosh algorithm) for remote servers.
- Keystroke-to-photon measurement at 0, 50 and 150 ms RTT.

**Phase 6 — browser blocks** (CEF on macOS, server-held state, tunnels)

**Phase 7 — platform**
- Lua config/actions, OSC 7501 UI (already parsed by libghostty: `GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS`), agent
  skill, login server, sharing, iOS.

---

## 10. Decisions

| Question | Decision |
|---|---|
| Snapshot accuracy | Use upstream `ghostty_snapshot_*` + continuation + write-until-ground. No custom state dump. Bump the Ghostty pin to current main. |
| Terminal size | Block ownership: owner's size and theme apply; app takes on focus/input; passive clients request; fall back to the most recent candidate. |
| Control protocol | Length-prefixed JSON-RPC frames over unix/iroh streams, schemas from `schemars`, seq'd events, 8 MiB overflow closes the connection. A separate per-block binary data stream authorized by single-use tickets. |
| GPUI pin | `nitishxyz/zed@b36b2e82` (`torrz/glass`); already contains desktop-platform's rev. |
| Platforms | macOS first (client + server). Linux server in Phase 4. |

## 11. Name ideas

**weft**, **moor**, **tether**, **hearth**, **loom** (loom is crowded). Check crates.io, domains and
trademarks before choosing.
