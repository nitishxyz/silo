# Phase 5: latency (predictive local echo)

Goal: typing into a remote block feels local. Keystrokes show up in the next frame at any RTT, underlined until
the server confirms them, and wrong guesses roll back without leaving anything behind. We measure this with a
keystroke-to-photon harness at 0, 50 and 150 ms RTT.

Planning doc. It builds on PLAN.md §3.3 (data plane) and the code as of the Phase 4 head.

## 1. Where things live

| Piece | Location | Why |
|---|---|---|
| Prediction engine (pure logic) | `silo-client/src/predict.rs` (new) | Sits next to `Mirror`, needs no GPUI, unit-testable headless |
| Hooking it to the stream | `silo-client/src/mirror.rs` | `Mirror::apply` is the one place output lands, so predictions are judged right after each frame |
| RTT estimator | `silo-client/src/predict.rs` (`Rtt`) | The transport's path RTT, read with every input (§4) |
| Overlay paint | `silo-render/src/surface.rs` (`Overlay.predicted`) | Same place as search matches and links; painted on top of the prepared frame |
| Key → prediction | `silo-app/src/pane.rs` (`encode_key`, `commit_text`, `send_input`) | That's where the keystroke and its encoded bytes are both known |
| TTY mode + input acks | `silo-server/src/block.rs`, `silo-pty`, `silo-proto/src/data.rs` | Only the server can read termios and order it against output |

The prediction never touches the mirror's `TerminalCore`. The mirror stays an exact copy of the server stream, so
resume and checkpoint (`Mirror::coherent`) still hold. Predictions are a separate overlay of cells plus a cursor.

## 2. Protocol additions (data plane, `term/1`)

Two new server→client frames. They are ordered with `OUTPUT` on the same stream, so each one has an exact place
in the byte stream. `Input` is unchanged on the wire.

| Frame | Payload | Meaning |
|---|---|---|
| `INPUT_ACK` | `count: u64, offset: u64` | The server wrote the first `count` `Input` frames from this subscriber to the PTY while the ring end was `offset`. Their echo, if any, is at `>= offset`. Cumulative; sent at most once per actor turn |
| `TTY_MODE` | `offset: u64, echo: bool, icanon: bool` | The PTY line discipline flags (`tcgetattr` on the master) changed at this point. Sent after `Welcome` and on every change |

- `count` is per subscriber: the actor counts `Input` messages per `sub_id`, so the wire needs no sequence numbers.
- **How `INPUT_ACK` relates to `Ack`:** they're independent. `ClientData::Ack(offset)` is client→server flow
  control (the client has applied output up to here). `INPUT_ACK` is server→client and tells the client where the
  echo of a given input starts. Prediction never changes `Ack`: the ack still covers exactly the output bytes the
  mirror applied.
- **Epoch:** predictions belong to the data-stream epoch (`Welcome.epoch`). A new `Welcome` (resume or snapshot),
  `SnapshotHead`, `Resized`, `Configured` or `Exited` drops every prediction. A resume restarts `count` at 0 on
  both sides, because it's a new subscriber.
- **Termios read:** `silo-pty` gains `Pty::line_mode() -> (echo, icanon)` (rustix `tcgetattr` on the master fd).
  The actor calls it after every `Input` write and every output chunk, which is one syscall and cheap. It emits
  `TTY_MODE` only when the value changes.
  - Phase 0 of task P5-1 checks that `tcgetattr` on the *master* reflects the slave's flags on macOS (it does on
    Linux).
  - If it doesn't on macOS, the actor keeps a slave fd open just for `tcgetattr`. The launcher already opens the
    slave, so it's one extra dup.
- **Why not a `BlockInfo` field:** a shell flips ECHO/ICANON on every prompt, which would spam `block.changed`
  events and arrive unordered relative to output. The flag only matters to an attached data stream, so it lives
  there. `terminal.size`-style inspection can add `tty_mode` to `SizeInfo` later if an agent needs it.

## 3. Prediction algorithm (Mosh, adapted to an exact mirror)

State per pane: `Predictor { epoch, confirmed_epoch, cells: Vec<Pred>, cursor: Option<(row,col)>, sent: u64,
acked: (count, offset), rtt: Rtt }`, where `Pred = { row, col, text, input: u64 }` (`input` = 1-based `Input`
frame count that carried it).

- **What is predicted:**
  - Printable text of width 1 is overwritten at the predicted cursor, and the cursor advances. That covers
    keystrokes and IME commits; wide or ambiguous-width graphemes start a new epoch instead.
  - Backspace (`0x7f`/`0x08`) moves left and blanks the cell, but only over cells this epoch predicted.
  - Anything else ends the epoch with no prediction: control bytes, Enter, escape sequences (arrows, kitty-encoded
    keys), paste, mouse, and focus reports. The predicted cursor becomes unknown.
  - Never past the right margin (no wrap guessing), and never while the viewport is scrolled back.
- **Epochs (Mosh's "tentative" rule):** after an epoch boundary, predictions are kept but *not shown* until one of
  them is confirmed. Then the whole epoch shows immediately. That stops a burst of wrong guesses after Enter
  (for example the first keys of a password, or a TUI starting).
- **Judging** runs after every `Mirror::apply` batch. A prediction for input `n` becomes judgeable once
  `acked.count >= n` and `mirror.next_offset > acked.offset`, meaning the echo window has started. Then:
  - **Confirmed:** the mirror cell at `(row,col)` holds `text`. It's removed from the overlay (the real cell now
    shows it) and `confirmed_epoch = epoch`.
  - **Wrong:** the cell differs and either `next_offset` has moved past the echo window (more output arrived after
    a quiet period of one frame) or `max(2*srtt, 50 ms)` passed since the `INPUT_ACK`. In that case the **whole
    epoch rolls back** (every pending `Pred` dropped, cursor prediction cleared) and a new epoch starts.
  - **Expired:** if nothing judges a prediction within `max(4*srtt, 250 ms)` of sending, it's dropped quietly (for
    example a program that buffers its echo).
- **Rollback is free:** the overlay only paints *over* the mirror's cells. Dropping a `Pred` brings back the real
  cell at the next paint, with nothing to undo.
- **Cursor:** while any shown prediction exists, the painted cursor is the predicted one. Otherwise it's the
  mirror's.

## 4. When prediction is on

Prediction is on only when **all** of these hold. There is no user setting; this is the one path.

1. **Remote connection with SRTT ≥ 30 ms.** It turns off below 20 ms (hysteresis). Local unix connections never
   predict.
2. **Not alt screen and no mouse tracking.** This covers vim/htop/less, whose echo can't be guessed safely.
   - `silo_vt::Frame` gains `alt_screen: bool` (read from Ghostty's mode state next to `mouse_tracking`).
3. **Not a no-echo prompt:** prediction is off when `icanon && !echo`. That's the signature of
   password prompts (`sudo`, `ssh`, `read -s`).
   - Line editors (zsh ZLE, readline, fish) run with `!icanon && !echo` and echo the text themselves. Those are
     predicted and checked by confirmation.
   - `echo && icanon` (a cooked `cat`) is predicted too.
4. **Block running and pane `Live`**, the same gate as `send_input`.

Turning off drops every prediction at once.

**RTT:**
- **Source:** `RemoteConn::rtt()` (new in `silo-transport`) returns the selected path's smoothed RTT from iroh 1.3
  `Connection::paths()` (QUIC's estimator, RFC 9002). Read at attach and again with every `Input` sent, so a
  connection that comes up over a relay and moves to the LAN stops predicting with the next key.
- **Not input-ack timing:** an earlier version sampled `Input` → `INPUT_ACK` with RFC 6298 smoothing, seeded once
  at attach. On a LAN that kept predicting (underline flicker, an occasional rollback) for the dozen-odd keys the
  smoothing took to forget a relay-time seed, and the samples included the time a busy UI thread took to read the
  ack.
- **No extra ping RPC:** an idle session doesn't need predictions, and QUIC keeps its estimate current.

## 5. Look

- Shown, unconfirmed cells are painted as an overlay:
  - the cell background in the frame's default background;
  - the glyph in the default foreground;
  - a 1 px underline in `theme::muted()` at the cell's underline position.
  The underline is the only visual cue (Mosh's).
- The predicted cursor uses the normal cursor style, with no extra decoration.
- Rollback just stops painting the overlay. No flash or animation.
- Nothing in the titlebar or status. Concise UI: the user should only notice that typing is fast.
- **Dev/test knob, no UI:** `SILO_PIN_PATH=1` in the app's environment sets `NetOptions.pin_initial_path` on its
  iroh endpoint (and turns off its relay and mDNS), so a remote host dialed through the `silo-latency` delay
  proxy, or on loopback, stays on that path instead of holepunching around it. That is how to see predictions
  in the GUI at a known RTT.

## 6. Keystroke-to-photon harness

**Latency injection: a UDP delay proxy.** A new crate, `crates/silo-latency` (bin + lib):
- It binds a UDP socket and forwards datagrams to the server endpoint's direct address. Each direction is delayed
  by `rtt/2` with a 1 ms jitter knob and no loss by default.
- The client dials an `EndpointAddr` whose only direct address is the proxy, with `RelayConfig::Disabled` and mDNS
  off, so iroh can't route around it.
- This keeps real QUIC behaviour: ACK pacing, congestion control and path RTT all see the delay. A frame-level
  delay shim wouldn't.
- The 0 ms run goes through the proxy too (same code path); a separate local unix baseline is reported alongside.
- Cross-check once by hand with `dnctl`/`pfctl` dummynet (needs root, so not part of the harness).

**Two measurements:**

1. **Headless (CI-able), `silo-latency bench`:**
   - Starts a server in a temp home and a proxy, attaches a `silo-client` `DataStream` + `Mirror` + `Predictor`,
     and runs `cat` (cooked, echo on) and `zsh -f` (line editor).
   - Types 200 chars at 10 Hz plus one backspace burst.
   - For each key it measures:
     - `t_pred`: when the predictor first *shows* the cell;
     - `t_echo`: when the mirror first contains the cell;
     - mispredictions and rollbacks.
   - Runs a password-prompt script (`read -s`) and asserts zero cells were shown.
2. **In-app photon, `SILO_LATENCY_TRACE=<file>`:**
   - `silo-app/src/latency.rs` (new) stamps `Instant` in `Pane::route_key`.
   - In the pane's paint closure it records the first paint whose overlay or mirror frame contains that key's
     cell, as `t_paint_pred` and `t_paint_echo`. It writes JSONL.
   - Keys come from `silo-latency type --app`, which posts `CGEvent` keystrokes to the frontmost Silo window.
     That needs Accessibility permission, granted once by hand.
   - The paint timestamp is the end of the GPUI paint for that frame. Display scan-out (one vsync, about 8–16 ms)
     isn't observable from the app; it's reported as a fixed note, not measured.

**Report:** `target/latency/report.md`, plus the raw JSONL next to it. There's one row per (RTT ∈ {0, 50, 150},
program ∈ {cat, zsh}) with:
- p50/p95/p99 of `key→pred` and `key→echo` (headless);
- p50/p95/p99 of `key→paint_pred` and `key→paint_echo` (app);
- the misprediction count and rate.

**Targets** (the acceptance gate for Phase 5):
- `key→paint_pred` p95 ≤ 20 ms at 50 and 150 ms;
- `key→paint_echo` p95 ≤ RTT + 20 ms;
- misprediction rate < 1 % on the scripted `zsh` run;
- 0 cells shown on password prompts.

## 7. Tasks (in order; file ownership doesn't overlap)

**P5-1: server line mode + input acks.** Depends on nothing.
- **Files:** `silo-proto/src/{data.rs,lib.rs (kinds)}`, `silo-pty/src/lib.rs`, `silo-server/src/block.rs`.
- **Work:**
  - `ServerData::{InputAck{count,offset}, TtyMode{offset,echo,icanon}}` with frame kinds;
  - `Pty::line_mode()`;
  - a per-subscriber input count and coalesced `INPUT_ACK`;
  - `TTY_MODE` after `Welcome` and on change.
- **Tests:**
  - proto round trip;
  - a block test where `stty -echo` gives `TTY_MODE{echo:false}` at an offset ≥ the output before it;
  - three `Input`s give a final `INPUT_ACK{count:3}` whose offset is ≤ the offset of the echoed bytes;
  - a macOS test that `tcgetattr` on the master tracks the slave.

**P5-2: predictor + RTT.** Depends on P5-1.
- **Files:** `silo-client/src/{predict.rs (new),mirror.rs,data.rs,lib.rs}`, `silo-transport/src/net.rs`
  (`RemoteConn::rtt`), `silo-vt/src/{model.rs,sys.rs}` (`Frame.alt_screen`).
- **API:**
  - `Predictor::input(&mut self, bytes: &[u8], frame: &Frame)` takes the *encoded* bytes, so the app and the
    headless bench share it;
  - `Predictor::server(&mut self, &ServerData)`;
  - `Predictor::judge(&mut self, frame: &Frame, next_offset)`;
  - `Predictor::overlay() -> (Vec<PredCell>, Option<cursor>)`.
  - `Mirror` exposes `next_offset()` and keeps no predictor state.
- **Tests (unit, with a real `TerminalCore`):**
  - echo confirms;
  - a non-echo (`read -s` in cooked mode) never shows;
  - a wrong echo (the program uppercases) rolls back the whole epoch;
  - a tentative epoch stays hidden after Enter until a confirm;
  - alt screen disables and clears predictions;
  - backspace only touches predicted cells;
  - RTT smoothing with hysteresis;
  - `Resized`/`Welcome` clear predictions.

**P5-3: overlay + app wiring.** Depends on P5-2.
- **Files:** `silo-render/src/surface.rs` (`Overlay.predicted`, `Overlay.predicted_cursor`, paint),
  `silo-app/src/pane.rs`.
- **Work:**
  - The pane owns a `Predictor`. `send_input` feeds it, `apply` feeds and judges it, and render passes the overlay.
  - It's gated on `Remote` control connections (§4.1).
- **Tests:**
  - render unit tests: overlay geometry, and the underline only on predicted cells;
  - pane test: a local connection never predicts.
- **Manual:** type over a 150 ms proxy and see underlined text that clears on echo.

**P5-4: latency harness.** Depends on P5-3.
- **Files:** `crates/silo-latency/**` (new), `silo-app/src/latency.rs` (new), plus two hook calls in
  `silo-app/src/pane.rs`. These land after P5-3 merges, so there's no concurrent edit.
- **Tests:**
  - the proxy adds RTT ±2 ms, measured on an echo stream;
  - `silo-latency bench --rtt 0,50,150` writes the report and enforces the §6 targets (non-zero exit on miss).

P5-1 and the proxy part of P5-4 can run in parallel; everything else is sequential.

## 8. Risks

- **`tcgetattr` on the macOS master:** verified in P5-1 (with the slave-fd plan as the alternative, §2).
- **Right prompts / autosuggestions** (zsh `RPROMPT`, fish suggestions) redraw cells near the cursor.
  - Judging only looks at the predicted cell, so a suggestion painting *after* it doesn't cause a rollback.
  - A suggestion that *overwrites* it does, which is the correct outcome.
- **Synthetic `CGEvent` typing** needs Accessibility permission: the app photon run is manual/nightly, not CI.
  The headless bench is the CI gate.
- **Paint timestamp ≠ photon:** vsync and compositor latency aren't measured (§6). If ever needed, a high-speed
  camera run can calibrate the constant.
