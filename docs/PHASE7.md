# Phase 7: platform (access, one API, block status)

Goal: every client, person and agent goes through one set of access rules and one API, and Silo tells you what
the programs in your blocks are doing.
- **Access rules:** grants `{who, scope, rights}` with expiry and revocation, checked in one place.
- **One API:** every app command is a control method; the CLI and later tooling come from `api.describe`.
- **Block status on the server:** OSC 7501 / 9;4 / 9 / 777 / 133, shown in panes, tabs, the switcher, the dock
  and notifications on every device.
- **Prompt features**, **Silo-to-Silo sharing** and **agent safety** are built on those three.

Planning doc. It builds on PLAN.md §3–4 and the code at `603cf74` (release 0.1.4). `GHOSTTY` below is
`target/ghostty/d2865b8f578fa4e85e2f2b85ef97f5ef69de951f/source` (its `include/ghostty/vt/terminal.h` is
byte-identical to the `.zig-cache/o/*/ghostty/vt/terminal.h` copy).

## 1. What exists today

| Thing | Finding |
|---|---|
| Who is calling | `Peer::{Local, Remote{endpoint_id, conn_id}}` (`silo-server/src/auth.rs:29`). Local = same-uid unix socket = owner. No principal type beyond that |
| Paired devices | `Vec<PairedDevice>` sealed in `state/devices.sealed` (`auth.rs:20`, `:62`). `authorize` admits a paired endpoint or redeems an invite (`auth.rs:145`); `revoke` closes the device's live connections (`auth.rs:184`) |
| Invites | Memory-only, single-use, ≤ 7 days (`auth.rs:23`, `:47`, `:130`). `InvitePurpose::{Device, Join}` (`silo-proto/src/control.rs:539`). A join invite gives `Admission::Join`, which may only call `host.offer` once (`auth.rs:55`) |
| Device proofs | A process with its own endpoint speaks for a paired device key: `device_of` verifies `ClientHello.device` (`auth.rs:89`, `control.rs:47`); `speak_for` re-keys the live connection so revoking the device closes it (`auth.rs:205`, called at `conn.rs:187` and `silo-server/src/upgrade.rs:58`). Proofs never redeem invites (`conn.rs:183-185`) |
| Method checks | One function, `permitted` (`conn.rs:346`): join connections only `host.offer`; `server.shutdown` and `pair.*` are local-only; everything else is open to paired devices. A second ad-hoc list makes methods `read_only` during upgrade (`conn.rs:287`) |
| Streams | Data and upload streams redeem a single-use ticket bound to the issuing connection (`registry.rs:826`, `:838`; `conn.rs:563`, `:645`), with no rights of their own. Tunnels admit self or any paired device (`tunnel.rs:25`). Upgrade streams admit paired devices (`silo-server/src/upgrade.rs:51`) |
| Events | `emit` to every subscriber, `emit_owner` to unix clients only (`registry.rs:341`, `:346`); per-client feature filter `needed_feature` is empty (`registry.rs:167`) |
| API | `Method` enum (`control.rs:150`), `METHODS` names/descriptions (`control.rs:329`), `api.describe` schemas and examples (`silo-proto/src/api.rs:16`), a test that every method describes (`api.rs:242`). The CLI has `silo api list/describe/call`, but `call` parses into the CLI's own `Method` (`silo-cli/src/main.rs:622`), so a newer server's methods can't be called |
| Compat | PROTOCOL=1, additive rules and `feature` names (`silo-proto/src/compat.rs:1-25`, `feature` is empty). `protocol.json` pins every schema; `SILO_UPDATE_PROTOCOL=1` rewrites it (`silo-proto/tests/protocol.rs:138`). `upgrade.json` freezes the upgrade stream (`silo-proto/src/upgrade.rs:314`) |
| App commands | 58 commands in one table (`silo-app/src/commands.rs:64-131`), run by `Workspace::execute` (`silo-app/src/palette.rs:162`). Many are client-side math over the layout, then `layout.ratio`/`window.select` (`palette.rs:207-237`); appearance, hosts, settings, full screen etc. exist only in the app |
| Forwarding to clients | `clipboard.read` goes to the block's attached data streams, first answer wins, 3 s (`conn.rs:472`, `block.rs` `BlockMsg::ClipboardRead`). The app answers only right after a paste gesture in that block (`silo-app/src/pane.rs:978`). `ui.open_session` is a broadcast event (`conn.rs:519`) |
| Server terminal hooks | `attach()` installs WRITE_PTY, COLOR_SCHEME, BELL, TITLE_CHANGED, PWD_CHANGED, CLIPBOARD_WRITE only (`silo-vt/src/adapter.c:145-151`). Replies from `write_output` go to the PTY (`silo-server/src/block.rs:1432-1436`); the client mirror drops them (`silo-client/src/mirror.rs:229`) |
| libghostty callbacks | PROGRESS_REPORT (OSC 9;4), DESKTOP_NOTIFICATION (OSC 9/777), PROGRAM_STATUS (OSC 7501), SEMANTIC_PROMPT (OSC 133), RESET (`GHOSTTY/include/ghostty/vt/terminal.h:101-107`). 7501 record rules: `terminal.h:1322-1346`; untrusted text: `:1348-1353`. The terminal **stores no reports** (`:1322`) |
| 7501 support query | Answered by libghostty itself through WRITE_PTY once a PROGRAM_STATUS callback is set (`GHOSTTY/src/terminal/stream_terminal.zig:222-228`, `:977`); reply body `query_reply` (`GHOSTTY/src/terminal/osc/parsers/program_status.zig:86-104`) |
| OSC 133 in the grid | Row prompt flags and cell semantic content are part of the screen and of GHOSTSNP (`GHOSTTY/src/terminal/snapshot/grid.zig:259`, `:278`). Jump: `GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT` (`terminal.h:332`). Output selection: `GHOSTTY_SELECTION_GESTURE_BEHAVIOR_OUTPUT` (`selection.h:356`). Stable row anchors: `grid_ref_tracked.h`. `Frame.at_prompt` is already read (`silo-vt/src/model.rs:212`) |
| Shell wrappers | zsh `ZDOTDIR` files, bash `--rcfile`, fish `vendor_conf.d` (`silo-server/src/shell.rs:61`, `:85`, `:104`); they only fix `PATH`. No OSC 133 |
| Block events | `Note::{Changed, Exited, Bell, Offer, Dirty}` (`block.rs:318`); `block.bell` is emitted (`registry.rs:854`) and ignored by the app (`silo-app/src/workspace.rs:458`). `Checkpoint` carries title/pwd but no status (`block.rs:296`) |
| Attribution | `ClientHello.client_name` (`control.rs:31`) → `ClientEntry.label` (`registry.rs:143`, `conn.rs:218`) → `BlockInfo.owner_name` (`silo-proto/src/model.rs:381`), shown as "sized by …" (`pane.rs:1641`) |
| Foreground process | `Pty::foreground_pid` (`tcgetpgrp` on the master, `silo-pty/src/lib.rs:460`), `Pty::pid` (`:448`), `process_name` (`:546`). The actor's `probe()` reads them every `PROBE_INTERVAL` = 400 ms (`block.rs:40`, `:1093`, `:1834`) for the title and cwd only. `BlockHandle.command` marks blocks started with an explicit command (`block.rs:333`) |
| Closing | App ⌘W → `close_block_id` → `block.close` with no check (`silo-app/src/workspace.rs:973`). Palette confirm only for `CloseSession`, `RestartServer`, `StopServer` (`commands.rs:154`, `palette.rs:146-149`). CLI `silo kill` → `session.close` (`silo-cli/src/main.rs:556`), `silo kill-server` → shutdown (`:587`). Closing the last app window quits the app (`silo-app/src/main.rs:160`); the server and its processes keep running |
| Web | libghostty-vt has a WASM surface (`GHOSTTY/include/ghostty/vt/wasm.h`). iroh 1.3.0 browser support is unverified |

## 2. Decisions

1. **Access rules first.** A grant is `{who, scope: server|session|block, rights: view<interact<control<owner}` with
   expiry and revocation. Every method declares its need in one table in `silo-proto`; the server checks it in one
   place. Local owner, paired devices, invites, join invites and device proofs become instances of the model.
   Data, upload, tunnel and upgrade streams use it too. A login server fits without redesign; it isn't built.
2. **One API.** Every palette command maps to a control method (existing, new server method, or `app.*` forwarded
   to a connected app). The CLI (`silo do`) and later Lua/MCP/agent tooling are generated from `api.describe`.
3. **Block status lives on the server**, in the server terminal's callbacks, kept exactly by the 7501 rules,
   sent as its own seq'd event, included in `session.view`, never bumping the session revision.
   It includes `busy`: whether a program is running, decided by the server from OSC 133 marks and the PTY
   foreground process group (§5.7).
4. **Notifications:** the server broadcasts; each device decides (Settings globally, Hosts tab per host).
5. **OSC 133:** the server's shell wrappers emit marks. Marks live in both terminals' grids (they are terminal
   state and travel in snapshots); command records (exit, duration, command line) live on the server.
   **Closing a busy block asks first** (§7.5): one dialog on the closing device, enforced by the server, `--force`
   in the CLI, and a setting to turn it off.
6. **Sharing** is a grant with a bearer secret, carried in a `silos1…` ticket in a URL fragment that works for Silo
   and the future web client.
7. **Agent safety:** full access by default; a fixed list of owner actions needs human approval in a Silo app;
   optional scoped tokens use the grant mechanism; the client name carries the agent's name.
8. **Agent safety is a guardrail, not a boundary.** Same-uid code can read Silo's state files and run any binary;
   approvals stop well-behaved agents from doing dangerous things by accident, nothing more.

**Deferred:**
- **Agent skill:** writes itself once `silo do` and `api.describe` are complete; nothing to design.
- **MCP server:** a thin generator over `api.describe` + access info; after the API is whole.
- **Lua config/actions** (Luau via mlua): settings stay data; Lua is for keybindings, commands, layouts and hooks,
  and needs the complete API first.
- **Web client:** a separate client project (iroh WASM + libghostty-vt WASM); Phase 7 only fixes the link format and
  runs the spike (P7-8).
- **Mobile/iOS client:** a separate client project; it consumes the same API, status and notifications.
- **Login server** (one root daemon hosting several unix users, each with their own server; SSH-key auth): the
  model reserves `who: account` and `Peer::Login` (§3.7); nothing else.

## 3. Access rules

### 3.1 Model (`silo-proto/src/access.rs`, new)

```rust
pub enum Rights { View, Interact, Control, Owner }           // Ord: View < Interact < Control < Owner
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Scope { Server, Session { session_id: SessionId }, Block { block_id: BlockId }, #[serde(other)] Unknown }
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Who {
    Device { id: String },                 // iroh EndpointId of a paired device key
    Secret { kind: SecretKind, hash: String }, // sha256 hex of a bearer secret; SecretKind: token | link
    Account { name: String },              // reserved for the login server (§3.7)
    #[serde(other)] Unknown,
}
pub struct Grant {
    pub id: String,                        // 16 hex
    pub who: Who, pub scope: Scope, pub rights: Rights,
    pub label: String,                     // "work laptop", "share with Ana", "claude-code"
    pub created_at: u64, pub created_by: String,   // creator's client label
    pub expires_at: Option<u64>,           // unix secs; None = until revoked
    pub last_seen: u64,
}
```

Rules:
- **Grants are immutable.** Changing one is revoke + create. Streams can then cache rights for their lifetime,
  because revocation closes them.
- **Coverage:** `Server` covers everything; `Session` covers that session and every window/block in it, including
  later ones; `Block` covers that block only.
- **Owner** is valid only with `Scope::Server` and `Who::{Device, Account}`. Secrets are never owner.
- A session or block that closes removes the grants scoped to it.
- **Invites** stay memory-only and single-use (`auth.rs:47`), but are now "a promise of a grant": a device invite
  yields `Grant{who: Device(redeemer), scope: Server, rights: Control}` and a join invite yields the
  connection-only principal `Offer`. It may call `host.offer`, once, and nothing else.

### 3.2 Principals: today's cases as instances

`Principal` is resolved once per connection at the hello and stored in `ClientEntry.principal`:

```rust
pub enum Principal { Owner, Grant(Grant), Offer { invite_id: String } }
```

| Today | Resolution | Principal |
|---|---|---|
| Unix socket, same uid (`Peer::Local`) | implicit, not stored | `Owner` |
| Unix socket + `ClientHello.grant = "silot1…"` (agent token) | hash → `Who::Secret{token}` grant | `Grant` (downscoped) |
| Paired endpoint (`authorize`, `auth.rs:145`) | `Who::Device{id}` grant | `Grant` (server, control) |
| Device proof (`device_of`, `auth.rs:89`) | proof → device id → same as above; `speak_for` keeps revocation | `Grant` |
| Device invite redeemed | creates the device grant, then as above | `Grant` |
| Join invite redeemed | none stored | `Offer` |
| Remote + `ClientHello.grant = <link secret>` (share) | hash → `Who::Secret{link}` grant | `Grant` |
| Login server (later) | `Peer::Login{account}` → `Who::Account` grants | `Grant` |

`ClientHello` gains `#[serde(default)] grant: Option<String>` (bearer secret) and
`#[serde(default)] role: ClientRole` (`cli` default, `app`, `#[serde(other)] unknown`). A remote hello must carry
exactly one of: a paired endpoint/proof, `invite`, or `grant`.

### 3.3 The need table: one table, one check

```rust
pub enum Target { Server, Session(&'static str), Window(&'static str), Block(&'static str), Blocks(&'static str) }
pub enum Need {
    Open,                                                    // any principal except Offer
    Offer,                                                   // only Principal::Offer
    Rights { on: Target, rights: Rights, approval: bool },   // Target names the param holding the id
    AppReply,                                                // only the app client the request went to
}
pub fn need(method: &Method) -> Need   // exhaustive match: a new Method without an entry does not compile
```

- **Check site:** `silo-server/src/access.rs` (new) `check(principal, role, &Need, &Registry) -> Result<Gate,
  RpcError>`. It resolves window → session and block → session, then compares rights. `conn.rs` calls it where
  `permitted` is called today (`conn.rs:267`). `permitted` and the upgrade `read_only` list (`conn.rs:287`) are
  deleted; read-only becomes `rights == View`.
- **Errors:** `forbidden` (`control.rs:446`) for insufficient rights, `not_found` for ids outside the principal's
  coverage (no existence leak), `unauthorized` for an `Offer` principal calling anything but `host.offer`.
- **`api.describe`** gains `access: {target, rights, approval, app}` (`MethodDescription`, `control.rs:612`), so
  generated tooling knows what a call needs.

| Need | Methods |
|---|---|
| Open | `server.info`, `api.list`, `api.describe`, `ping`, `events.subscribe`, `session.list` (filtered to visible sessions), `blocks.busy` (filtered to viewable blocks, §7.5), `approval.answer` (handler checks approver eligibility, §9) |
| Offer | `host.offer` |
| AppReply | `app.reply` |
| view · block | `block.view`, `terminal.capture`, `terminal.size`, `data.connect`, `block.commands`, `terminal.command_output` |
| view · session | `session.view` |
| interact · block | `terminal.send`, `terminal.resize`, `session.own_block`, `session.disown_block`, `clipboard.read` (gesture-gated, §9), `clipboard.write`, `browser.*` (`browser.report` keeps its owner-client check), `block.focus`, `block.focus_step`, `block.focus_dir`, `block.status_seen` |
| interact · session | `window.select`, `window.step`, `ui.open_session` |
| control · session | `session.rename`, `session.close`, `window.create/close/rename/move`, `block.split/close/zoom/swap/float/move`, `layout.ratio/grow/edge/equalize`, `terminal.clear`, `terminal.reset` |
| control · server | `session.create`, every forwarded `app.*` except below |
| owner · server | `server.log`, `pair.list`, `grant.list` |
| owner · server + approval | `server.shutdown`, `server.restart`, `pair.create`, `pair.revoke`, `grant.create`, `grant.revoke`, `app.hosts_add`, `app.hosts_remove` |

Multi-block methods (`session.own_block {block_ids}`) need the right on every id. Paired devices keep exactly what
they have today (control at server scope: everything except owner methods), so nothing regresses.

### 3.4 Streams

| Stream | Need | Enforcement |
|---|---|---|
| Data (`DATA_HELLO`) | view · block | `issue_ticket` records the principal's rights on the block in `Ticket`; `Attach` gains `rights`. Below interact, `Input`, `Resize`, `Config`, `ClipboardPush` and `ClipboardContent` are dropped (`conn.rs:595-618`) and the client never becomes owner or candidate |
| Upload (`UPLOAD_HELLO`) | interact · block | checked at redeem (`conn.rs:645`) |
| Tunnel (`silo/tunnel/1`) | control · server | `tunnel.rs:25` asks `access` with the endpoint's principal |
| Upgrade (`UPGRADE_HELLO`) | control · server + approval | `upgrade.rs:51`; the hello is frozen (`upgrade.json`), approval only delays `HOST_BUILD`; denial is `UpgradeRejected{code:"denied"}` |
| Local handoff (`SILO-UPGRADE` magic, `conn.rs:102`) | uid check only, no approval | it is the install path (`silo up`, `install.sh`) and only runs a binary the user installed |

**View-only and size:** a view principal's desired size is recorded but never applied, even on an unowned block, and
`own_block` is forbidden. That extends today's ownership rule (PLAN §3.6) to viewers.

### 3.5 Event visibility

`Registry::emit_to` (`registry.rs:350`) takes a `Visibility` instead of `owner_only`, and each client's principal
decides:

| Visibility | Events |
|---|---|
| Server-wide, filtered per client | `sessions.changed` (summaries limited to visible sessions; serialized per principal class) |
| Session | `session.changed`, `session.closed`, `session.watchers`, `ui.open_session` |
| Block | `block.changed`, `block.bell`, `block.status`, `block.attention`, `block.command_finished`, `browser.command` |
| Owner | `host.offered` (today's `emit_owner`), `grants.changed` |
| Approvers | `approval.requested`, `approval.resolved` (§9) |
| One client | `app.request` (§4.2) |

Block-scoped principals get no `session.*` events and can't call `session.view`; they use `block.view` and block
events. That avoids per-principal pruned layouts.

### 3.6 Storage, expiry, revocation

- `state/grants.sealed` (same `HostKey`/AAD scheme as `auth.rs:20`) holds `Vec<Grant>`. On open, an existing
  `devices.sealed` is converted to device grants (label = device name, `rights: control`) and deleted; devices that
  already have a grant are skipped, so one an older server wrote after an aborted upgrade still merges. One-way;
  a downgraded server sees no devices.
- Secrets are 32 random bytes and are shown once. Only `sha256` is stored.
- `Auth.live` (`auth.rs:68`) maps `conn_id → (grant_id, RemoteConn)`. Local token clients are tracked by grant id
  in the registry. Revocation and expiry close every connection of that grant (remote: `close(b"revoked")`; local:
  `ClientEntry.close`).
- A 60 s sweeper removes expired grants and closes their connections. `last_seen` keeps today's debounce
  (`auth.rs:25`).

### 3.7 Room for the login server (not built)

- **Transport:** a root daemon authenticates SSH keys and forwards to the per-user server over a dedicated socket.
  The per-user server trusts only that socket, and it yields `Peer::Login{account}`.
- **Model:** `Who::Account{name}` grants let other accounts into sessions. Owner = the account that maps to the
  server's uid.
- **Today:** `Who::Account` parses and round-trips; `grant.create` rejects it with
  `bad_request("accounts need a login server")`.

### 3.8 Protocol additions

- **Methods:**
  - `grant.create {scope, rights, kind: "token"|"link", ttl_secs?, label?} → {grant, secret, ticket?}`. `ticket`
    is the `silos1…` share ticket for `kind: link`. `ttl_secs` defaults to 1 day for links and none for tokens;
    the maximum is 30 days.
  - `grant.list {} → {grants}` and `grant.revoke {id}`.
  - `pair.create/list/revoke` stay as they are (compat), implemented on device grants.
- **Event:** `grants.changed {grants}` (owner).
- **Feature:** `compat::feature::ACCESS = "access"`. A client lacking it never sees the new events (the
  `needed_feature` filter, `registry.rs:167`).

## 4. One API for every kind of automation

### 4.1 Command → method

Every `Command` (`commands.rs:64-131`) gets `fn api(self) -> Api` with `enum Api { Method(&'static str),
Cli(&'static str) }`. New server methods do the math the app does today in `palette.rs:207-237`; the pure layout
functions move from `silo-app/src/{layout.rs,focus.rs}` to `silo-proto/src/layout_ops.rs` (new), so app and server
share them.

| Commands | Method |
|---|---|
| NewSession · RenameSession · CloseSession | `session.create` · `session.rename` · `session.close` |
| NewTab · NewBrowserTab · CloseTab · RenameTab | `window.create` · `window.create{kind:browser}` · `window.close` · `window.rename` |
| NextTab · PreviousTab | **new** `window.step {session_id, delta: i32}` |
| NewPane · SplitRight · SplitDown · NewBrowser | `block.split`; `axis` becomes optional, omitted = along the longer side in pixels (`TermSize` cell px); `kind:browser` |
| ClosePane · ZoomPane · FloatPane · DockPane | `block.close` · `block.zoom` · `block.float` · `block.move{dock}` |
| FocusNextPane · FocusPreviousPane | **new** `block.focus_step {block_id, delta: i32}` |
| FocusPaneLeft/Right/Up/Down | **new** `block.focus_dir {block_id, direction}` |
| GrowPane · ShrinkPane | **new** `layout.grow {block_id, delta: f32}` |
| MovePaneEdgeLeft/Right/Up/Down | **new** `layout.edge {block_id, direction}` |
| EqualizePanes | **new** `layout.equalize {window_id}` |
| OpenUrl | `browser.navigate` |
| ClearScrollback · ResetTerminal | `terminal.clear` · `terminal.reset` |
| StopServer · RestartServer | `server.shutdown` · **new** `server.restart {}` (shuts down, then re-execs its own binary; ends sessions) |
| ShowServerLog | **new** `server.log {lines?: u32} → {text}` |
| StartServer | `Cli("up")`: with no server there is no API to call. The only exemption |
| NewWindow · CycleWindows | `app.window_new {session_id?}` · `app.window_cycle {}` |
| SwitchSession · NextSession · PreviousSession | `app.switcher {}` · `app.session_step {delta}` (the current session is per app window) |
| AddHost · RenameHost · RemoveHost | `app.hosts_add {ticket?}` · `app.hosts_rename {host, name}` · `app.hosts_remove {host}`; plus `app.hosts_list` |
| Copy · Paste · SelectAll | `app.copy` · `app.paste` · `app.select_all` (each `{block_id?}`) |
| AppearanceComfortable/Compact | `app.appearance {density}` |
| Show/HidePaneHeaders · ToggleFullScreen | `app.pane_headers {visible}` · `app.full_screen {on?}` |
| OpenSettings · ReloadTerminalSettings · OpenTerminalSettingsFile | `app.settings_open {section?}` · `app.settings_reload` · `app.settings_open_file` |
| (theme, fonts, colors, every `SettingId`) | `app.settings_get {id?}` · `app.settings_set {id, value}` over `settings_table.rs` |
| CheckForUpdates · CommandPalette | `app.update_check` · `app.palette {query?}` |
| HideSilo · HideOthers · ShowAll · Quit | `app.hide` · `app.hide_others` · `app.show_all` · `app.quit` |
| Phase 7 commands (§7, §8) | `app.jump_prompt {delta}`, `app.select_output`, `app.copy_output`, `app.open_share {link}`; sharing uses `grant.create/revoke` |

Every `app.*` method has an optional `client_id` param (target override, §4.2).

### 4.2 Forwarding `app.*` to an app

- An app says `role: "app"` in its hello (§3.2).
- The server forwards the whole request as the event `app.request {request_id, request: {method, params}, caller:
  ClientLabel}`, sent to **one** client.
  - The app runs it through the same `Workspace::execute` path, then calls `app.reply {request_id, result?,
    error?}`.
  - The server answers the original call with that result, or `timeout` after 10 s.
- **Targeting:**
  1. `client_id` if given; it must be an app client, else `not_found`.
  2. Otherwise the app client that was **most recently active**: user input on a data stream, `own_block take` or
     `block.focus`, tracked as `ClientEntry.active_at`. Ties go to the most recently connected.
  3. Only app clients on the **unix socket** of this server (`origin: None`) are eligible. A remote server must
     never drive the user's local app (open hosts, quit, read settings).
- **None connected:** `RpcError{code:"no_app", message:"No Silo app is connected to this server"}`.
- **Feature:** `compat::feature::APP_REQUESTS = "app_requests"`. An app without it is not eligible.

### 4.3 Generated CLI

- `silo do <method> [--param value]… [--json PARAMS]`: fetches `api.list` + `api.describe` from the server it
  talks to and builds flags from the params schema (scalars, enums, `--param-json` for objects).
  - Required params can also be given positionally, in schema order. Output is the JSON result.
  - Unknown methods come from the server, not the CLI build, so newer servers work.
- `silo api call` sends `{method, params}` raw instead of parsing into the CLI's `Method` (`main.rs:622`).
- Hand-written subcommands (`silo ls/new/attach/send`) stay as ergonomic aliases.
- Lua/MCP (deferred) use the same `api.describe` + `access`.

### 4.4 Tests

- `silo-proto`: the exhaustive `need()` match, plus `every_method_declares_access` (each `METHODS` name has
  `describe(name).access`). `every_method_describes_and_its_example_validates` (`api.rs:242`) covers the new
  methods.
- `silo-app` `commands.rs`: `every_command_has_an_api_method`. For each `Command::ALL`, `api()` is a name in
  `silo_proto::control::METHODS`, or `Cli("up")` for `StartServer` only.
- Server: `window.step`/`block.focus_dir`/`layout.edge` give the same result as the app's old functions on fixture
  layouts (moved tests); `app.*` with no app gives `no_app`; two apps → the most recently active gets it; a
  remote app client never gets `app.request`.

## 5. Block status

### 5.1 Hooks (`silo-vt`)

- `attach()` (`adapter.c:136`) installs PROGRAM_STATUS, PROGRESS_REPORT, DESKTOP_NOTIFICATION, SEMANTIC_PROMPT
  and RESET **only on server terminals**: `silo_vt_new(…, hooks: bool)`, with `TerminalCore::server(..)` in
  `sys.rs`.
  - Client mirrors keep today's callbacks, so the support query is answered exactly once, by the server
    (`block.rs:1432` → PTY).
  - Hooked like that, `OSC 7501 ; ?` gets `query_reply` with no code of ours (`stream_terminal.zig:977`).
- **Ordered queue:** each callback copies its strings into one ordered `VtEvent` queue in `SiloVt`, capped at 1024
  events or 256 KiB; on overflow it keeps the newest and sets `lost`.
  `take_events` (`sys.rs:584`) returns them in order: `TerminalEvents.vt: Vec<VtEvent>`.

```rust
pub enum VtEvent {
    Status { state, kind, progress: i8, id, app, title, message },     // OSC 7501, CLEAR included
    Progress { state, percent: i8 },                                     // OSC 9;4
    Notify { title, body },                                              // OSC 9 / 777
    Prompt { kind, prompt_kind, exit: Option<i32>, command, error, row: TrackedRow },  // OSC 133
    Reset,                                                               // RIS
}
```

- Order matters. RIS delivers `Status{CLEAR, id:""}` before `Reset` (`terminal.h:1344-1346`), and a prompt start
  must clear `working` records reported earlier in the same chunk.

### 5.2 Records (`silo-server/src/status.rs`, new, pure, unit-tested)

`Records` follows `terminal.h:1322-1346` exactly:
- **One record per id** (`""` = root). A report **replaces** its record completely.
- **CLEAR** removes the id and every `id/…` beneath it; an empty id removes all.
- **App inheritance:** the effective `app` is the nearest ancestor's (`a/b/c` → `a/b` → `a` → `""`), computed when
  the record is published, never stored.
- **Prompt start** (`Prompt{PROMPT_START, PRIMARY}`) and **process exit** (`block.rs:1877` `finish`) remove
  `working`, `blocked` and `idle`. `done` and `error` stay until seen.
- **Cap 256**, evicting the least recently updated.
- **RIS** clears everything, including progress.

Progress (OSC 9;4) is one `Option<Progress>` per block. It is removed by `REMOVE`, prompt start, exit, RIS, or
15 s without an update (Ghostty's behaviour).

**Seen:** `block.status_seen {block_id, ids?}` removes the given `done`/`error` records, or all of them. The result
is broadcast, so seen is shared by every client and device. The app sends it after the block has been visible in
a key window for 1 s.

### 5.3 Protocol

```rust
pub struct ProgramStatus {
    pub id: String, pub state: StatusState, pub kind: Option<StatusKind>, pub progress: Option<u8>,
    pub app: String,      // effective (inherited)
    pub title: String, pub message: String, pub updated_at: u64,   // unix ms
}
pub enum StatusState { Idle, Working, Done, Blocked, Error, #[serde(other)] Unknown }
pub enum StatusKind { Permission, Question, Auth, #[serde(other)] Unknown }
pub struct Progress { pub state: ProgressState /* set|error|indeterminate|pause|unknown */, pub percent: Option<u8> }
pub struct BlockStatus { pub block_id: BlockId, pub records: Vec<ProgramStatus>, pub progress: Option<Progress> }
pub struct AttentionCounts { pub working: u32, pub blocked: u32, pub done: u32, pub error: u32 }
```

- **Events:**
  - `block.status {status: BlockStatus}`: the whole block status each time. State changes go out at once;
    progress-only changes are coalesced to at most 4/s per block. It **never** bumps `revision` and never emits
    `session.changed`.
  - `block.attention {block_id, session_id, session_name, block_title, reason, title, body, at}`, where `reason` is
    `blocked|done|error|notification`. Raised when a record enters blocked/done/error and on OSC 9/777;
    notifications are capped at 1/s per block (burst 5).
- **Views:** `SessionView` gains `#[serde(default)] status: Vec<BlockStatus>` (non-empty blocks only).
  `SessionSummary` (`model.rs:519`) gains `#[serde(default)] attention: AttentionCounts`. `sessions.changed` is
  re-emitted only when counts change, never on progress.
- **Feature:** `compat::feature::STATUS = "status"` gates the new events.

### 5.4 Untrusted text

- **Server:** `silo-proto/src/text.rs` (new) `untrusted(s, max_chars)` is applied before anything leaves it.
  - It strips C0/C1 and every Unicode `Cf` char (bidi overrides U+202A–202E, isolates U+2066–2069, U+200B–200F,
    U+061C, U+FEFF, U+00AD), collapses whitespace and truncates.
  - Limits: app 64, title 128, message 512, notification title 128 / body 1024, command line 4096.
- **Clients:** they always show program text next to a trusted label, `host · session · block title`, never alone
  (`terminal.h:1348-1353`).

### 5.5 Persistence and handoff

- `Checkpoint` (`block.rs:296`) gains `#[serde(default)] status: Option<StatusState>` (records + progress +
  command log, §7.3).
- **Hot upgrade:** the processes live on, so everything is carried as is (`handoff.rs` serializes `Checkpoint`).
- **Restart/crash restore:** the restored block has no process, so the exit rule runs at restore. Only `done` and
  `error` records survive, which is the right outcome: unseen results stay visible.

### 5.6 UI (app)

- **Pane header:**
  - working: spinner, with `NN%` when known;
  - blocked: "needs you" accent dot and the kind icon (permission/question/auth);
  - done: check; error: red mark.
  - Hovering shows each record as app · title · message, under the trusted label. OSC 9;4 shows as a thin bar
    under the header.
- **Tabs:** the strongest state across the window's blocks (blocked > error > working > done).
- **Session switcher:** `AttentionCounts` per session as small badges.
- **Dock badge:** blocked + error across all connected hosts (`NSApp.dockTile.badgeLabel`), cleared at 0.

### 5.7 Busy blocks

**Shape:** `BlockStatus` gains `#[serde(default)] busy: Option<Busy>`.

```rust
pub struct Busy {
    pub process: String,          // "vim"; through `untrusted`, 64 chars
    pub command: Option<String>,  // the OSC 133 command line, when the shell sent one
    pub since: u64,               // unix ms when this busy period began
}
```

**Rule** (`silo-server/src/status.rs` `busy()`, pure; inputs gathered by the actor). Only a `Running` terminal block
can be busy; exited, restored and browser blocks never are. A running block is busy when **any** of these holds:

1. **Marks:** the shell's last OSC 133 step is C (output start) with no D since. Marks are reset at A and at RIS.
   This is the only way to see a builtin running (`read x`, a `while` loop with a builtin body), because then the
   foreground pgrp is the shell itself.
2. **Foreground:** `foreground_pid()` ≠ `pty.pid()` (the shell) and `process_name(fg)` is not harmless.
3. **Command block** (`BlockHandle.command`): there is no shell, so `pty.pid()` is the program. The block is busy
   while it runs, unless `process_name(fg or pid)` is harmless.

Marks decide whenever the shell sends them; rule 2 still applies on top of them. A program running in the
foreground may forward another shell's marks: `ssh` to a host with shell integration passes through the remote
prompt's A. Without rule 2, a running `ssh` would then look idle.

**Harmless names: decided, built-in and short.** Only shells: `sh bash zsh fish dash ksh mksh tcsh csh nu xonsh
elvish login`, compared after stripping a leading `-`. A shell at its prompt has nothing in flight, so a nested
`bash` or `zsh` doesn't prompt. Everything else does, including `ssh`, `less`, `man` and `tail -f`. A guess list
is wrong for someone (an `ssh` session, a pager over a long query), and confirming costs one keystroke. There is no
user-editable list in Phase 7; the deferred Lua hooks can add one later.

**`process`:** the foreground process's name. When the foreground is the shell (rule 1 with a builtin), it is the
first word of the command line, falling back to `"a command"`.

**Freshness:**
- Marks update `busy` at once.
- The foreground check runs in `probe()` every 400 ms (`block.rs:1093`).
- A close check asks each target actor directly (`BlockMsg::Busy {reply}`), which re-probes first, so a program
  started 100 ms ago is never missed.

**Publishing:**
- A change in `busy` (start, end, or a different `process`) emits `block.status`, with no revision bump.
- `since` survives a handoff through the `Checkpoint` status (§5.5). A restored block has no process and no `busy`.

## 6. Notifications

- **Server:** broadcasts `block.attention` to every client with view on the block. It never decides.
- **Device policy (app, client-side):**
  - Settings › Notifications: on/off, per reason (needs you · done · error · program notifications), and "only
    when the block isn't visible" (default on).
  - Hosts tab: per-host on/off (`HostInfo`, `silo-app/src/hosts.rs:184`).
- **Delivery:** `UNUserNotificationCenter`.
  - Title = trusted label (`host · session · block`), subtitle = app/title, body = message.
  - Identifier = `host/block/record-id`. When a later `block.status` shows the record gone (seen elsewhere), the
    app removes the delivered notification, so seen syncs across devices.
- **Permission:** requested at the first notification that policy would show, not at launch. If it's denied,
  Settings › Notifications says so and links `x-apple.systempreferences:com.apple.Notifications-Settings.extension`.
  Unbundled dev builds skip delivery and log once.
- **Click:** `userInfo {host endpoint id, session_id, block_id}`. The app activates, switches host and session,
  focuses the block, and the 1 s visibility rule sends `block.status_seen`.
- Bell stays ignored (`workspace.rs:458`).

## 7. Prompt features (OSC 133)

### 7.1 Shell marks (`shell.rs`)

The wrappers add marks after the user's files ran, interactive shells only:

| Shell | A (prompt) | B (input) | C (output) | D (end) |
|---|---|---|---|---|
| zsh (`zsh_file`, `.zshrc`/`.zlogin` stage) | `precmd` | appended to `PS1` as `%{…%}` | `preexec`, `cmdline_url=` | `precmd`, with `$?` |
| bash (`bash_rc_script`) | `PROMPT_COMMAND` | end of `PS1` (`\[…\]`) | `PS0` (bash ≥ 4.4) with `cmdline_url=` | `PROMPT_COMMAND`, with `$?` |
| fish (`fish_snippet`) | `fish_prompt` event | — | `fish_preexec` | `fish_postexec`, with `$status` |

- macOS `/bin/bash` 3.2 has no `PS0`: it gets A/B/D, with no duration and no command line. No DEBUG-trap emulation.
- fish 4+ emits OSC 133 itself; the snippet checks `status features`/version and adds nothing there (verified in
  P7-4).
- Users' own integrations (starship, Ghostty, iTerm2) may emit marks too. Repeated marks are harmless
  (`terminal.h:1524-1526`).

### 7.2 Where marks live: decided

- **Prompt/input/output marks** are terminal state. libghostty stores them per row and cell in both the server
  terminal and the client mirror, and GHOSTSNP carries them (`grid.zig:259`, `:278`). So:
  - jump to previous/next prompt is client-local (`DELTA_PROMPT`, `terminal.h:332`);
  - select/copy one command's output is client-local (`SELECTION_GESTURE_BEHAVIOR_OUTPUT`, `selection.h:356`);
  - snapshot/restore, resume and hot upgrade keep them with no extra work.
- **Command records** (exit, duration, command line) are not in the grid. The server owns them, because it sees
  every byte from the start and has real timestamps.

### 7.3 Command records

- **Server:** `silo-server/src/prompts.rs` (new) keeps `CommandLog` per block: the last 1000
  `CommandRecord {index, command, exit, started_at, duration_ms}` (index monotonic per block).
  - C starts a record (`started_at`, command line through `untrusted`); D finishes it.
  - D without C makes a record with no duration.
  - At D the server pins a tracked grid ref on its own terminal (`grid_ref_tracked.h`) at the cursor row.
- **Row anchors to clients (data plane, ordered with output):**
  - `COMMAND_END = 0x8f`, JSON `{offset, record, rows_from_bottom}`. It is sent right after the `Output` frame that
    contained the D mark. `rows_from_bottom` is computed after that whole chunk is applied, so it addresses the
    same line in the client's identical mirror.
  - `COMMANDS = 0x90`, JSON `[{record, rows_from_bottom}]`. It is sent right after `SnapshotHead`, for every
    record whose anchor is still in the terminal, in snapshot coordinates.
  - The client pins tracked refs when the row exists: at once for the active area, and when its history page
    arrives for older rows.
  - Old clients skip unknown kinds (`compat.rs:9`).
- **Control:**
  - `block.commands {block_id, limit?} → {records}`;
  - `terminal.command_output {block_id, index?} → {text, record}` (default = last finished; text from the server
    terminal's OUTPUT selection);
  - event `block.command_finished {block_id, session_id, record}`.
- **Persistence:** `CommandLog` is in `Checkpoint` with `rows_from_bottom` per record, re-pinned at restore. RIS
  drops anchors and keeps records.
- **RESET** also clears `osc_title`/`osc_pwd` and calls `probe()`. libghostty doesn't report cleared title/pwd
  through the change callbacks (`terminal.h:1554-1556`), so today a reset leaves a stale title.

### 7.4 UI and CLI

- **Commands:**
  - `JumpToPreviousPrompt`/`JumpToNextPrompt` (⌘↑/⌘↓) → `app.jump_prompt`;
  - `SelectCommandOutput` (⇧⌘A) → `app.select_output`;
  - `CopyCommandOutput` (⌥⌘C) → `app.copy_output`.
- **Gutter:** a 2 px red tick on the D row of commands with a non-zero exit. Hover shows `exit 1 · 2.3 s ·
  <command>`. Successful commands get no mark.
- **CLI:**
  - `silo wait <block> [--timeout S]` waits for the next `block.command_finished` and exits with its code.
  - `silo run <block> -- <command…>` = `terminal.send` + wait + print `terminal.command_output`, then exit with the
    command's code. This is the agent-facing path.

### 7.5 Confirm before closing a busy block

**What asks:** closing a pane (`block.close`), a tab (`window.close`) or a session (`session.close`), and stopping
or restarting the server (`server.shutdown`, `server.restart`). These are the actions that end processes.

**Quit and closing an app window don't ask.** They end nothing: the server keeps every session running
(`main.rs:160` only quits the app). Asking there would teach users to click through the dialog.

**Protocol (additive):**
- `block.close`, `window.close`, `session.close`, `server.shutdown` and `server.restart` gain
  `#[serde(default)] force: bool`.
- New method `blocks.busy {session_id?, window_id?, block_ids?} → {busy: Vec<BusyBlock>}`. All params omitted
  means the whole server. Results are filtered to blocks the caller can view.

  ```rust
  pub struct BusyBlock {
      pub block_id: BlockId, pub session_id: SessionId, pub window_id: WindowId,
      pub session_name: String, pub block_title: String,   // block_title through `untrusted`
      pub busy: Busy,
  }
  ```
- `RpcError` (`control.rs:406`) gains `#[serde(default, skip_serializing_if = "Option::is_none")] data:
  Option<Value>`.
- **Refusal:** a close without `force` that would end busy blocks fails with code `busy`, the message and
  `data: {busy: [BusyBlock]}`. Nothing is closed: the operation is all or nothing.
  - Message for one block: `vim is running in 'api'`.
  - Message for several: `3 programs are running: vim in 'api', cargo in 'build', ssh in 'prod'` (at most 3 named,
    then `and N more`).
- **Feature** `compat::feature::CLOSE_GUARD = "close_guard"`. The server enforces the guard only for clients that
  advertise it, so older apps and CLIs close as they do today (PROTOCOL=1 additive rule, `compat.rs:1-14`).
- **Single check site:** `silo-server/src/close_guard.rs` (new) `check(registry, targets, force, client) ->
  Result<(), RpcError>`. `conn.rs` calls it before those five handlers, after the access check.

**App flow** (`silo-app/src/close_confirm.rs`, new):

1. Close Pane / Close Tab / Close Session / Stop Server / Restart Server call `blocks.busy` for their target.
2. If anything is busy, **one** dialog opens. It lists every busy block (process, block title, session › tab
   when it spans several; at most 8 rows, then "and N more") and shows the command line on hover. The buttons are
   **Close** (default **Cancel**; ⌘⌫ confirms).
   - For the destructive commands (`commands.rs:154`), this dialog *is* their confirmation, with the busy list
     added. There is never a second dialog.
   - With nothing busy, non-destructive closes go ahead at once, and destructive ones show today's confirmation.
3. After confirming, the close is sent with `force: true`. Without confirmation it is sent with `force: false`.
   If a block became busy in between, the server answers `busy` and the dialog opens again from `error.data`.

**Setting:** Settings › General › "Confirm before closing running programs" (default on, a `settings_table.rs`
row). When it's off, the app skips `blocks.busy` and always sends `force: true`. Destructive-command
confirmations stay.

**Another device closing:** the check runs in the caller's request, so the dialog appears on the device doing the
closing: a phone, a second Mac, or a shared-link client with control rights. Other devices only see the block go
away.

**CLI:**
- `silo kill <session>` and `silo kill-server` gain `--force`. The generated `silo do block.close` etc. get
  `--force` from the schema.
- **On `busy`:**
  - If stdin is a TTY, the CLI prints the list and asks `Close anyway? [y/N]`, then retries with `force: true`.
  - Otherwise it exits 1 with `silo: vim is running in 'api'; pass --force to close it anyway`.
  - Agents get the same refusal unless they pass `--force` on purpose. This is a guard against accidents, not
    security (decision 8).

## 8. Silo-to-Silo sharing

- **Share:** "Share Session…" / "Share Block…" → `grant.create {kind: link, scope, rights: view|interact, ttl}`.
  The dialog shows the link, the expiry and a QR code, and warns that **interact on a shell is shell access as
  you**.
- **Ticket:** `silos1` + lowercase unpadded base32 of postcard `ShareTicket {addr: EndpointAddr, secret: [u8; 32],
  vt: String}`. `1` is not a base32 digit, so it can't be confused with a `silo1` pairing ticket. `vt` lets a web
  loader pick the libghostty build before connecting (VT must match exactly, `compat.rs:48`).
  - **Decided (§13):** `addr` carries the endpoint id and **only the relay URL**, no direct addresses: a browser
    can't use them and a shared link would leak them. Silo apps still move to a direct path after dialing.
- **Link:** `https://<SHARE_ORIGIN>/#silos1…`. The ticket is in the fragment, so it never reaches an HTTP server.
  - Silo accepts the full link or the bare ticket in "Open Shared Link…" (`app.open_share`). macOS can't claim
    https links without a domain association, so there is no `silo://` scheme.
  - The web client (deferred) reads `location.hash`.
- **Redeeming:** the client dials `addr` with its own endpoint, `ClientHello.grant = base32(secret)`, `role: app`.
  Link grants are **bearer secrets, reusable** (not single-use) until expiry or revocation, because a browser
  endpoint changes per page load (decided, §13).
  - The shared session appears in the app as a host entry marked "shared", showing only what the grant covers.
  - Session shares see the session; block shares see one block, through `block.view` and its data stream.
- **Who is watching:** `SessionView` gains `#[serde(default)] watchers: Vec<Watcher {client_id, label, principal:
  owner|device|token|link, rights, blocks}>`, plus the event `session.watchers {session_id, watchers}`. A watcher is
  a client with a data stream attached to a block of the session. No revision bump.
  - The titlebar shows the labels of non-owner watchers; the pane header shows "watched by …" for blocks with
    link watchers.
- **Viewers and size:** view principals never own or resize (§3.4). Interact principals use today's ownership.
- **Revocation:** "Stop Sharing" (`grant.revoke`) closes every connection at once. Expiry does the same within 60 s.
- **Tunnels** need control · server, so a shared browser block of `localhost` doesn't load for link viewers.
  Accepted for Phase 7.
- **Relays:** a web viewer is relay-only. If the server runs with relays disabled, the share dialog says the link
  works only for Silo on the same network.

## 9. Agent safety

- **Default:** zero config. Anything on the unix socket is `Owner`, including agents in panes and the CLI.
- **Approval** applies to methods marked `approval` (§3.3) and upgrade streams, when the caller is **not** an app
  client. The app's own calls are the human's.
  1. The server holds the call and emits `approval.requested {id, action, summary, caller: {label, from?}, expires_in_ms}`
     to eligible approvers. `summary` is server-written ("Create a device invite valid for 10 min"); `from` is the
     calling process's pane when it runs inside one (§9 attribution).
  2. An approver answers with `approval.answer {id, allow}`. `approval.resolved {id, allowed}` dismisses the card in
     other apps.
  3. The call fails with `forbidden` "Denied in Silo", or `timeout` after 120 s.
  - **Approvers:** app clients whose principal holds the method's rights at server scope and that advertise
    `compat::feature::APPROVALS = "approvals"` (P7-5): an app that shows no cards is never asked, so no call waits
    120 s for an answer that cannot come.
  - **`clipboard.read` asks nobody: the paste gesture is the approval** (`pane.rs:978`). The attached apps answer a
    read only right after the user pasted into that block, else nothing within 3 s, exactly as before Phase 7. It
    needs interact on the block, so a scoped agent token can still read the images pasted into its pane.
- **UI:** a card in the focused window ("claude-code wants to create a pairing invite · from work › api"), with
  Allow and Deny. If Silo isn't frontmost, a notification is shown too.
- **Scoped tokens:**
  - `silo grant --session X --interact [--ttl 8h] [--label claude-code]` → `grant.create {kind: token}` prints
    `SILO_TOKEN=silot1…`.
  - The CLI sends `SILO_TOKEN` as `ClientHello.grant`, and the connection is downscoped to that grant.
  - `silo grants` lists grants; `silo grants revoke <id>` revokes one. Tokens use the same grant mechanism as
    sharing.
- **Attribution:**
  - The CLI sets `client_name = "silo cli · $SILO_AGENT"` when `SILO_AGENT` is set. `label()` (`conn.rs:314`) shows
    it, so it lands in `owner_name` and the "sized by" badge (`pane.rs:1641`), watchers and approval cards.
  - The server also records the caller's pane: unix peer pid (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux),
    walked up the parent chain to a block's `pid`. It shows as `from: session › block title`.
- **Not done:** no per-agent sandboxing, no default restriction, no prompts for normal work (typing, splitting,
  closing).

## 10. Open questions

1. **No app connected, owner action needs approval. Decided: B, restricted** (below). With no app able to approve,
   the server answers `confirm` (`data: {approval, summary}`) only to a caller outside every pane of this server
   (§9 peer-pid ancestry finds no block) that has a controlling tty. The CLI asks `Allow? [y/N]` on `/dev/tty`,
   answers with `approval.answer`, and repeats the call, which consumes that one confirmation. Anyone else gets
   `forbidden` "Approve in Silo: no Silo app is connected".
   - **A.** Fail: `forbidden` "Approve in Silo: no Silo app is connected".
   - **B.** Fall back to a y/N confirmation the CLI reads from `/dev/tty`.

   Risks:
   - An agent in a Silo pane has a PTY, so it can drive a tty prompt itself, e.g. `script -q /dev/null silo pair`
     with `y` piped in. A tty answer doesn't prove a human.
   - Under A, the linking flows that run on hosts with no app break: `silo up` over SSH mints a device invite
     (`silo-cli/src/main.rs:793-803`), and so does `silo join` (`silo-cli/src/link.rs:65`). So does `silo pair` on
     a headless server.

   **Chosen: B, restricted.** Allow the tty confirmation only when the calling process is **not** inside
   any pane of this server (the §9 peer-pid ancestry check finds no block) and has a controlling tty. Otherwise A.
   That keeps SSH setup working and closes the in-pane agent case. Residual risk: an agent that SSHes to another
   host and runs `silo` there answers that host's prompt itself. So does one that double-forks a `silo` call so it
   is reparented to init: the ancestry walk then finds no pane, while the pane's terminal is still its controlling
   tty (a guardrail, decision 8).
2. **`SHARE_ORIGIN`:** which domain hosts the web client (and so the share links). The format is fixed regardless.
3. **iroh 1.3.0 in the browser:** P7-8 must confirm `wasm32-unknown-unknown` + relay-only dialing of a native
   endpoint with ALPN `silo/ctl/1` and extra bi streams before the ticket format is frozen in `protocol.json`.

## 11. Tasks

Ownership is exclusive among tasks that run at the same time. A file passes to a later task only once its owner
has merged.

**P7-1: protocol + access rules.** First, alone; everything depends on it.
- **Files:**
  - `silo-proto/src/{access.rs (new), text.rs (new), control.rs, api.rs, compat.rs, model.rs, data.rs, lib.rs}`,
    `silo-proto/protocol.json`, `silo-proto/tests/protocol.rs`;
  - `silo-server/src/{access.rs (new), grants.rs (new), approval.rs (new), auth.rs, conn.rs, registry.rs,
    tunnel.rs, upgrade.rs, lib.rs}`;
  - `silo-cli/src/{main.rs, lib.rs}` (`silo grant(s)`, `SILO_TOKEN`, `SILO_AGENT`, raw `api call`).
- **Work:**
  - **All** Phase 7 protocol: every method, event, field, data kind and feature in §3–§9.
  - Methods owned by later tasks are dispatched from `conn.rs` to `fn handle` in `silo-server/src/{app_bridge,
    status, prompts, layout_cmds}.rs`, created here as stubs returning `method_not_found`.
  - `conn.rs` calls `close_guard::check` before the five close handlers (§7.5). `close_guard.rs` is created here
    as a stub that always allows.
  - Grants, principals, the need table, stream checks, event visibility, approvals (server side) and the
    `devices.sealed` conversion.
- **Acceptance:**
  - the `need()` match is exhaustive and `every_method_declares_access` passes;
  - a rights matrix test covers owner / device / session-view link / session-interact link / block-view link /
    token × one method per row of §3.3;
  - a revoked or expired grant closes its control and data connections within 60 s;
  - a view stream's `Input` never reaches the PTY;
  - tunnel and upgrade use `access`;
  - an existing `devices.sealed` home opens with the same devices;
  - `pair.create` from a non-app client emits `approval.requested` and resolves on `approval.answer`;
  - `protocol.json` is updated additively; `cargo test -p silo-proto -p silo-server -p silo-cli` passes.
- **As built** (where it differs from the plan above):
  - `force` on `block.close`/`window.close`/`session.close`/`server.shutdown` is left to P7-9 (`server.restart` has
    it): existing callers construct those variants, and the field is additive whenever it lands.
  - `COMMAND_END`/`COMMANDS` kinds and their payloads (`data::{CommandEnd, CommandAnchor}`) are pinned in
    `protocol.json`; `ServerData` decodes them with P7-3/P7-6, so clients skip them as unknown until then.
  - The view-only rule is enforced where data frames are read (`conn.rs`); `Attach` is unchanged.
  - Upgrade approval is asked once `UPGRADE_BINARY` arrives (a build check asks nothing), and an upgrade goes ahead
    as before when no app can approve: headless hosts keep `silo remote upgrade`.
  - Approvals are built and tested (`Config::require_approval`) but stay off in `silo server` until P7-5: today's
    app shows no cards, so an approval-gated call from a pane (`silo pair`, `silo down`, `silo remote install`)
    could only wait or fail. P7-5 turns them on with the cards.
  - `clipboard.read` is interact · block with no approval card (§9): the paste gesture is the approval.
  - `grant.create {kind: link}` returns `ticket: null` until P7-7 adds `ShareTicket`; the secret is returned.
  - Block-scoped principals list no sessions in `session.list`.

**P7-2: one API.** After P7-1. Runs in parallel with P7-3 and P7-4.
- **Files:** `silo-proto/src/layout_ops.rs (new)`; `silo-server/src/{app_bridge.rs, layout_cmds.rs}`;
  `silo-app/src/{commands.rs, palette.rs, layout.rs, focus.rs, app_requests.rs (new)}`;
  `silo-cli/src/{main.rs, generated.rs (new)}`.
- **Work:**
  - the §4.1 server methods, `server.restart` and `server.log`;
  - forwarding + targeting;
  - the app executes `app.request` and replies;
  - `Command::api()`;
  - `silo do`.
- **Acceptance:**
  - `every_command_has_an_api_method`;
  - moved layout tests pass in `silo-proto`;
  - `silo do window.step --session-id S --delta 1` switches tabs in a running app;
  - `silo do app.full_screen` toggles the app;
  - with no app connected → exit 1 with `no_app`;
  - a remote app client is never targeted.

**P7-3: server terminal hooks (status, notifications, command records).** After P7-1.
- **Files:** `silo-vt/src/{adapter.c, adapter.h, sys.rs, model.rs, lib.rs}`, `silo-server/src/{status.rs,
  prompts.rs, block.rs}`, `silo-server/src/registry.rs` (status in view/summaries, visibility).
- **Work:**
  - §5.1–§5.5 and §7.3, including `COMMAND_END`/`COMMANDS` frames, `block.commands`,
    `terminal.command_output` and the RESET title fix;
  - §5.7 busy detection: `status.rs` `busy()`, the harmless list, `BlockMsg::Busy`, and `busy` in `block.status`.
- **Acceptance (unit + block tests):**
  - every rule in §5.2 has its own test (replace, subtree clear, app inheritance, prompt-start clear, exit clear,
    done kept until seen, 256 LRU eviction, RIS);
  - `printf '\e]7501;?\e\\'` in a block gets the `query_reply` bytes on its input;
  - progress ticks emit `block.status` without changing `revision`;
  - bidi/zero-width chars are stripped;
  - a hot-upgrade handoff keeps working records, and a crash restore keeps only done/error;
  - after a snapshot attach, `COMMANDS` rows point at the D lines (checked against `capture`);
  - busy, without marks (`bash --norc`): `sleep 30` → `busy{process:"sleep"}` within one probe; a nested `bash`
    at its prompt → not busy;
  - busy, with marks: C then `read x` (foreground = shell) → `busy{process:"read", command:"read x"}`; D clears it;
  - a foreground child that prints `ESC]133;A` while running stays busy (the ssh case);
  - a command block running `sleep` is busy, and one running `zsh` is not;
  - busy changes emit `block.status` without changing `revision`.
- **As built** (where it differs from the plan above):
  - `TerminalCore::server(size)` = `new` + `hook()` (`silo_vt_hook`), not a `hooks` flag on `silo_vt_new`: restored
    server terminals (`restore_vt`) are hooked the same way.
  - `VtEvent::Prompt { step, prompt_kind, exit, command, error, row: Option<TrackedRow> }`: rows are tracked at the
    cursor for C and D only. `TerminalEvents` lost `Clone`/`PartialEq` (it now owns tracked rows).
  - `Checkpoint.status` is `Option<SavedStatus>` (records, progress, busy, open C mark, `CommandLog`, D and C
    anchors as `rows_from_bottom` + column), not `Option<StatusState>`. `spawn_resumed` (restart with a fresh
    shell) runs the exit rule like `spawn_restored`; `spawn_adopted` carries everything, `busy.since` included.
  - `rows_from_bottom` counts from the primary screen's last row and exists only while the primary screen is
    active: `COMMAND_END`, `COMMANDS` and checkpoints leave out anchors while the alternate screen is up.
    `COMMANDS` is sent only when non-empty.
  - `terminal.command_output` is Ghostty's OUTPUT selection from the C cell, cut before the D cell (Ghostty keeps
    output semantics until the next A). An A without D drops the running record; a second C replaces it.
  - "Progress-only" means the records' `(id, state, kind)` and `busy` are unchanged; those changes are coalesced to
    4/s. A 7501 attention's `title` is the record title (else its app), `body` its message.
  - `compat::FEATURES` now lists `status` (this server emits the events; this build's clients advertise it).
    `block.view` returns the block's status.
  - `blocks.busy` stays with P7-9 (§11); `BlockMsg::Busy` re-probes and answers for it.
  - The "without marks" busy test runs bash through an rcfile laid out like Silo's hook (a login-shell block,
    so rule 2 is exercised) rather than `bash --norc` (which would be a command block, rule 3).

**P7-4: shell marks.** After P7-1, in parallel with P7-3.
- **Files:** `silo-server/src/shell.rs`.
- **Acceptance:**
  - extend `login_shells_lead_path_with_silo_after_user_files` (`shell.rs:187`) to assert A/B/C/D sequences, with
    the exit code and `cmdline_url`, for zsh, bash (≥ 4.4 and 3.2 where present) and fish;
  - no marks for non-interactive shells;
  - no double C/D on fish 4+.
- **As built** (where it differs from §7.1):
  - The wrappers add no marks when a user integration already emits them: zsh/bash check function bodies and
    `PS1`/`PS0`/`PROMPT_COMMAND` for `133;` after the user's files ran; fish checks the `fish_prompt` function and
    `fish_prompt`/`fish_preexec`/`fish_postexec` handlers. This also keeps a re-sourced wrapper from adding twice.
  - fish decides at the first prompt (vendor snippets run before `config.fish`). fish 4+ is left alone unless
    `mark-prompt` is off (`status test-feature`), in which case it gets the fish 3 marks.
  - D is sent only after a C: zsh sets a flag in `preexec`, bash ≥ 4.4 in `PS0` (an array subscript assignment, no
    fork). An empty line gives A/B and no D. bash 3.2 sends D at every prompt after the first, empty lines included.
  - bash ≥ 4.4 takes `cmdline_url` from `history 1` only when the line entered history (`HISTCMD` grew); a line
    left out (`ignorespace`, `ignoredups`, history off) gets C with no `cmdline_url`.
  - B is re-appended at each prompt (zsh: last `precmd` hook, bash: end of `PROMPT_COMMAND`), so prompt frameworks
    that rebuild `PS1` keep it. fish sends no B (fish 4.3+ sends its own).
  - fish runs `fish_postexec` for `exit` too, so a shell exit ends with D (the previous status).
  - Tested: zsh 5.9, bash 3.2.57 and 5.3.20, fish 3.7.1 and 4.9.3 (native and with `no-mark-prompt`), each on a
    real PTY. Extra shell builds are found through `SILO_TEST_SHELL_DIRS` (colon-separated, test only).

**P7-5: status, notification and approval UI.** After P7-2 and P7-3.
- **Files:** `silo-app/src/{pane.rs, titlebar.rs, switcher.rs, workspace.rs, notifications.rs (new), approvals.rs
  (new), settings_table.rs, settings_panel.rs, hosts.rs}`.
- **Turning approvals on** (P7-1 left them off, §11 P7-1 "As built"): `compat::feature::APPROVALS` in
  `silo-proto/src/compat.rs` and `FEATURES`, `Registry::approvers` requiring it (`silo-server/src/registry.rs`),
  and `require_approval: true` for `silo server` in `silo-cli/src/main.rs` once P7-9 has merged that file.
- **Acceptance:**
  - UI driver (`scripts/ui-driver.swift`): `printf` a blocked 7501 report → the header dot, tab mark, switcher count
    and dock badge appear;
  - focusing the block for 1 s clears done/error on a second connected app;
  - a notification click focuses the block;
  - per-host off suppresses notifications;
  - `silo pair` from a pane shows the approval card, and Allow returns the ticket.

**P7-6: prompt UI.** After P7-3, P7-4 and P7-5.
- **Files:** `silo-client/src/mirror.rs` (frames → tracked refs), `silo-render/src/surface.rs` (gutter mark),
  `silo-app/src/{pane.rs, commands.rs, palette.rs}`.
- **Acceptance:**
  - ⌘↑/⌘↓ jump between prompts after a fresh snapshot attach;
  - ⇧⌘A selects exactly the last output;
  - a failed command shows a red tick that survives app restart and server hot upgrade;
  - `silo run <block> -- false` exits 1.

**P7-7: sharing.** After P7-5 and P7-8.
- **Files:** `silo-transport/src/share.rs (new)` (`ShareTicket`), `silo-app/src/{share.rs (new), hosts_state.rs,
  add_host.rs}`; the watchers UI in `silo-app/src/titlebar.rs` (after P7-5).
- **Acceptance (two in-process servers, relay disabled):**
  - a view link attaches, can't type or resize, and shows in watchers;
  - an interact link types;
  - revoke closes it at once;
  - a block link can't `session.view`;
  - `silos1` and `silo1` tickets never parse as each other.

**P7-8: web spike.** Independent; start with P7-1.
- **Files:** `spikes/iroh-wasm/**` (new, outside the workspace).
- **Acceptance:** a written result in a new `docs/PHASE7.md` §13 "Spike result". A browser page dials a native
  `silo` server's `EndpointAddr` (relay-only) with `silo/ctl/1`, completes `ClientHello` with a link secret, opens a
  second bi stream for data, and decodes a snapshot with libghostty-vt WASM. Or: the specific blocker.

**P7-9: close guard, server and CLI.** After P7-2 and P7-3.
- **Files:** `silo-server/src/close_guard.rs` (replaces the P7-1 stub), `silo-cli/src/{main.rs, generated.rs}`.
- **Work:**
  - §7.5 server side: `blocks.busy`, the all-or-nothing `busy` refusal with `error.data`, enforcement gated on
    `close_guard`;
  - CLI `--force`, the TTY prompt, and the non-TTY refusal.
- **Acceptance (server tests with real PTYs + CLI tests):**
  - `block.close` on a block running `sleep 30` from a `close_guard` client gives `busy`, with `data.busy[0].busy.process
    == "sleep"`, and the block is still there; `force: true` closes it;
  - `window.close` on a tab with one busy and one idle pane gives `busy` and closes neither;
  - `session.close`, `server.shutdown` and `server.restart` behave the same;
  - a block that starts `sleep` 50 ms before the close is still caught (the fresh `BlockMsg::Busy` probe);
  - a client without `close_guard` closes as today;
  - `blocks.busy` from a block-scoped link lists only that block;
  - `silo kill S` with stdin not a TTY exits 1, with stderr `silo: sleep is running in '…'; pass --force to close
    it anyway`; `--force` closes it; under a pty, answering `y` closes it and the default (Enter) keeps it.

**P7-10: close confirmation in the app.** After P7-6 and P7-9.
- **Files:** `silo-app/src/{close_confirm.rs (new), palette.rs, workspace.rs, commands.rs, settings_table.rs,
  settings_panel.rs}`.
- **Acceptance (UI driver):**
  - ⌘W on a pane running `vim` shows "vim is running in '<title>'"; Cancel keeps it, Close closes it;
  - Close Tab with three busy panes shows one dialog listing all three;
  - Close Session with nothing busy shows only today's confirmation; with `cargo build` running, one dialog that
    includes it;
  - with the setting off, ⌘W closes at once;
  - closing from a second connected app (remote host) shows the dialog there and nothing on the first;
  - the race path: the server's `busy` answer to a `force: false` close reopens the dialog;
  - Quit and closing the last window never ask, and the processes are still running after relaunch.

Order: P7-1 → (P7-2 ∥ P7-3 ∥ P7-4 ∥ P7-8) → (P7-5 ∥ P7-9) → P7-6 and P7-7 → P7-10.
**Verifiable headless:** P7-1, P7-3, P7-4, P7-9, and P7-2's server side. P7-5, P7-6 and P7-10 need the GUI.

## 12. Risks

- **Approval friction:** every non-app owner action prompts. Mitigated by keeping the list short (§3.3) and the
  paste gesture for clipboard reads.
- **Spoofable `role: app`:** any same-uid process can claim to be the app and approve itself. This is accepted by
  decision 8, and the hello is not the security boundary.
- **Busy false positives:** a background job that takes the terminal (`fg`), or a program that `setpgid`s itself
  into the foreground, counts as busy. That is correct: closing would kill it.
  **False negatives:** a daemonized child that left the foreground group and a shell without marks running a
  builtin aren't seen. Neither is protected today either.
- **Per-client event filtering** serializes `sessions.changed` per principal class. Cheap at Silo's client counts;
  measured in P7-1.
- **Row anchors** depend on the mirror being exact, which the differential fuzz test already gates (GAPS §1).
  The known upstream reflow divergence with small byte-limited scrollback (GAPS §1 findings) can misplace exit ticks
  in that configuration only.
- **bash 3.2** gives no duration and no command line.
- **iroh WASM** may not support 1.3.0 relay-only dialing yet. Sharing between Silo apps doesn't depend on it; only
  the web client does.

## 13. Spike result

P7-8 (`spikes/iroh-wasm`, how to rerun in its README). **Everything works with iroh 1.3.0 as pinned, including
the libghostty-vt decode. No blockers.** One change to the link format is recommended (below).

**Setup:**
- Browser: headless Chrome 155 on macOS. Server: `target/debug/silo server --relay default` (0.1.4 + this tree,
  build `2182b42c`) in a temp `SILO_HOME` on the same Mac. Home relay `aps1-1.relay.n0.iroh.link`.
- Client: `iroh = "=1.3.0"` with `default-features = false, features = ["tls-ring"]` (no portmapper, metrics or
  fast-apple-datapath). Resolved: noq 1.3.0, iroh-relay 1.3.0, ring 0.17.14, wasm-bindgen 0.2.129. Built with
  rustc 1.99.0 (rustup) for `wasm32-unknown-unknown`, `Endpoint::builder(presets::N0)` unchanged.
- The secret used is a **device invite** (`silo pair` → `silo1…` in `ClientHello.invite`). Link grants don't exist
  until P7-1.

**What works (every step, every run):**
1. iroh builds for `wasm32-unknown-unknown` without patches. `ring`'s C code needs LLVM clang + `llvm-ar`
   (Homebrew `llvm`); with Apple clang/ar the link fails on undefined `ring_core_0_17_14_*` symbols.
2. The page dials the server's `EndpointAddr` with ALPN `silo/ctl/1`. The path is always
   `relay(aps1-1…)`. Dialing the ticket's full address (IPs included) works too: the browser just ignores
   the IPs.
3. `ClientHello` (`protocol: 1`, exact `vt`, invite) → `ServerHello` with the client id.
4. `session.list`, `session.view` and `data.connect` on the control stream.
5. A second bi stream: `DataHello` with the ticket → `Welcome{mode: snapshot}` → `SNAPSHOT_HEAD`
   (1,835 B, `GHOSTSNP`) + one `SNAPSHOT_HISTORY` (26 B) → `SNAPSHOT_DONE`. The data ticket is bound to the
   issuing connection (`registry.rs:838`). That holds here because both streams share one iroh connection.
6. **Stretch, done:** ghostty at the pinned revision builds libghostty-vt for wasm upstream
   (`zig build -Demit-lib-vt=true -Dtarget=wasm32-freestanding -Doptimize=ReleaseSmall
   -Dvt-features=-kitty-graphics`, zig 0.16.0). The module has no imports and exports the full C API plus
   `ghostty_wasm_*` helpers and `ghostty_type_json`.
   - The page decodes the received snapshot with `ghostty_snapshot_decoder_new_buf` + `_decode`, then formats it
     as plain text with `ghostty_formatter_*`. The text matches the server's screen and scrollback.
   - Decode takes 1.6–3.5 ms. Struct options are passed by pointer (wasm32 C ABI), using the offsets from
     `ghostty_type_json`.

**Bundle size** (no `wasm-opt`):

| Artifact | Raw | gzip -9 | brotli 11 |
|---|---|---|---|
| iroh client (`opt-level="z"`, LTO, 1 CGU, `strip`) | 2,820,318 | 1,036,356 | 807,359 |
| same without `strip` | 5,707,463 | 1,417,529 | 985,379 |
| wasm-bindgen JS glue | 46,843 | | |
| `ghostty-vt.wasm` (ReleaseSmall) | 854,193 | 289,168 | 228,630 |

About 1.04 MB brotli for both. Instantiating takes 12–19 ms.

**Connect time over relay** (relay RTT 165–310 ms on this network, 9 browser runs: 4 waiting, 5 not):

| Phase | Waiting for `online()` first | Dialing at once |
|---|---|---|
| own home relay (`online()`) | 1.73–1.99 s | — |
| `connect` (QUIC handshake via relay) | 175–201 ms | 507–544 ms |
| `ClientHello` → `ServerHello` | 198–267 ms | 255–360 ms |
| `session.list` | 162–278 ms | 162–171 ms |
| `DataHello` → `SNAPSHOT_DONE` | 177–194 ms | 165–189 ms |
| **link to first snapshot** | **2.86–3.13 s** | **1.47–1.61 s** |

- A web client should **not await `online()`**: the dial only needs the server's relay.
- After the handshake, each request costs one relay RTT.
- For comparison, the native build of the same code connects in 169 ms over the same relay-only address and
  then iroh moves it to a direct path (RTT 0.4 ms). The browser always stays on the relay.

**Link format:**
- **`silo1` invites decode in WASM unchanged** (postcard `EndpointAddr` from iroh-base 1.3.0, about 330 chars).
- **`silos1` + `ShareTicket {addr, secret, vt}` works as designed.**
  - Encoded and decoded in WASM: 360 chars. A link `https://silo.example/#silos1…` is 382 chars, fine for URLs
    and QR codes.
  - `vt` is needed. The page must send the exact `vt` in both hellos and load the matching `ghostty-vt.wasm`
    before connecting, so the web host should serve one module per VT (e.g. `/vt/<vt>/ghostty-vt.wasm`).
- **The `#` fragment works:** the static server's log shows only `GET /?vt=…`; the ticket never reached it.
- **Change: put only the relay URL in a share ticket's `addr`.** Today `EndpointAddr` carries every direct
  address: LAN, Tailscale and public IPv4/IPv6.
  - A browser can't use them, and a shared link would leak them.
  - Silo apps lose nothing: a relay-only dial was promoted to a direct path in the native run.
  - A ticket without a relay URL is unusable on the web; this matches §8's "Relays" rule.
  - Not tested: the N0 preset's HTTPS pkarr lookup for a server whose home relay changed after the link was made.
- **Device invites can't stand in for links.**
  - Each page load is a new endpoint, so every run paired a new permanent device: 16 "silo web spike" entries in
    `silo devices` after the spike.
  - The reusable bearer link grant from §8 / P7-1 is required.
  - `REMOTE_HELLO_MAX` (8 KiB) is ample for a hello carrying a link secret.
- **`silo-proto` doesn't build for wasm32:** `rustix` → `errno`, and the workspace `tokio` features `net`/`process`
  → `mio`. The spike mirrors the frames and JSON by hand.
  - A web client needs the wire types in a crate (or behind a feature) without tokio I/O and rustix.
  - `silo-transport` doesn't build either, for the same tokio reason.

**Newer iroh:** not needed. crates.io has 1.4.0, which wasn't tried.

**Recommendation:**
- Freeze the §8 format, with `ShareTicket.addr` limited to the server's relay URL (no IPs), and record that in
  `protocol.json` with P7-7.
- Keep iroh 1.3.0 for the web client.
- Build plan for the deferred web client:
  - LLVM clang for `ring`, and `strip` in the release profile;
  - dial without waiting for `online()`;
  - wasm-safe wire types split from `silo-proto`;
  - libghostty-vt's upstream wasm build, served per `vt`.
